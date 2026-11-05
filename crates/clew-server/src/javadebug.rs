//! `clew-server java-debug --java <java> --javac <javac>`: a debug adapter
//! for Java, over JDWP.
//!
//! Every JDK debugs through JDWP, the wire protocol of its debugging agent
//! (`-agentlib:jdwp`). This adapter speaks it directly, so a JDK is all a
//! Java program needs to be debugged — no language server, no plugin. On
//! `launch` the program runs suspended under the agent, which announces the
//! port it listens on; the adapter connects, and maps DAP onto JDWP.
//!
//! The program is a main class on a classpath: `mainClass` and `classPaths`
//! when the launch gives them (a Maven or Gradle build's output), else the
//! launched `.java` file compiled for the session (`javac -g`, its package's
//! source root on the source path, the build's output on the classpath).
//! Breakpoints on a class not loaded yet are set when it loads, and reported
//! then. Stepping skips the JDK's own classes. A frame's locals, `this`, an
//! object's fields, an array's elements and strings are read; JDWP evaluates
//! no expressions, so `evaluate` reads a variable and the fields and
//! elements along a path from it (`order.items[0].price`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use clew_core::framing::read_message;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, oneshot, watch};

use crate::dapout::{Out, forward};

/// How long compiling the program may take.
const COMPILE_TIMEOUT: Duration = Duration::from_secs(300);

/// How long the JVM has to say where its agent listens, and to connect.
const START_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a JDWP command may take to be answered.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the JVM has to exit once its session is over.
const EXIT_GRACE: Duration = Duration::from_secs(3);

/// The elements of an array, the fields of an object, listed at once.
const MAX_CHILDREN: usize = 100;

/// Largest JDWP packet read.
const MAX_PACKET: usize = 64 * 1024 * 1024;

/// Classes stepping does not stop in.
const NOT_STEPPED_INTO: &[&str] = &["java.*", "javax.*", "jdk.*", "sun.*", "com.sun.*"];

/// Run the adapter on this process's stdin/stdout.
pub async fn run(java: PathBuf, javac: PathBuf) -> Result<(), String> {
    serve(java, javac, tokio::io::stdin(), tokio::io::stdout()).await
}

/// [`run`], over the given streams.
pub async fn serve<I, O>(java: PathBuf, javac: PathBuf, input: I, output: O) -> Result<(), String>
where
    I: AsyncRead + Unpin + Send + 'static,
    O: AsyncWrite + Unpin + Send + 'static,
{
    let mut adapter = Adapter {
        java,
        javac,
        out: Out::new(output),
        vm: None,
        process: None,
        scratch: None,
        roots: Vec::new(),
        state: Arc::new(Mutex::new(State::default())),
    };
    let mut input = BufReader::new(input);
    while let Ok(Some(message)) = read_message(&mut input).await {
        if message["type"] != "request" {
            continue;
        }
        if !adapter.handle(&message).await {
            break;
        }
    }
    adapter.end().await;
    Ok(())
}

// -- JDWP --------------------------------------------------------------------

/// Sizes of the ids this VM uses (`VirtualMachine.IDSizes`).
#[derive(Debug, Clone, Copy)]
struct IdSizes {
    field: usize,
    method: usize,
    object: usize,
    reference: usize,
    frame: usize,
}

impl Default for IdSizes {
    fn default() -> Self {
        IdSizes {
            field: 8,
            method: 8,
            object: 8,
            reference: 8,
            frame: 8,
        }
    }
}

/// Where code is: a method of a class, at a bytecode index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Location {
    tag: u8,
    class: u64,
    method: u64,
    index: u64,
}

/// A value read from the VM: its tag and its bits (an object's id for the
/// reference tags).
#[derive(Debug, Clone, Copy, PartialEq)]
struct JValue {
    tag: u8,
    bits: u64,
}

/// A JDWP packet's data, written.
#[derive(Default)]
struct Writer {
    bytes: Vec<u8>,
    sizes: IdSizes,
}

impl Writer {
    fn new(sizes: IdSizes) -> Writer {
        Writer {
            bytes: Vec::new(),
            sizes,
        }
    }
    fn u8(mut self, v: u8) -> Self {
        self.bytes.push(v);
        self
    }
    fn i32(mut self, v: i32) -> Self {
        self.bytes.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn u64(mut self, v: u64) -> Self {
        self.bytes.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn id(mut self, v: u64, size: usize) -> Self {
        self.bytes.extend_from_slice(&v.to_be_bytes()[8 - size..]);
        self
    }
    fn object(self, v: u64) -> Self {
        let size = self.sizes.object;
        self.id(v, size)
    }
    fn reference(self, v: u64) -> Self {
        let size = self.sizes.reference;
        self.id(v, size)
    }
    fn method(self, v: u64) -> Self {
        let size = self.sizes.method;
        self.id(v, size)
    }
    fn field(self, v: u64) -> Self {
        let size = self.sizes.field;
        self.id(v, size)
    }
    fn frame(self, v: u64) -> Self {
        let size = self.sizes.frame;
        self.id(v, size)
    }
    fn string(mut self, v: &str) -> Self {
        self.bytes
            .extend_from_slice(&(v.len() as u32).to_be_bytes());
        self.bytes.extend_from_slice(v.as_bytes());
        self
    }
    fn location(self, l: Location) -> Self {
        self.u8(l.tag)
            .reference(l.class)
            .method(l.method)
            .u64(l.index)
    }
}

/// A JDWP packet's data, read.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
    sizes: IdSizes,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8], sizes: IdSizes) -> Reader<'a> {
        Reader {
            bytes,
            at: 0,
            sizes,
        }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.at.checked_add(n).filter(|&e| e <= self.bytes.len());
        let end = end.ok_or("a JDWP reply ended early")?;
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }
    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    fn i32(&mut self) -> Result<i32, String> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn id(&mut self, size: usize) -> Result<u64, String> {
        let bytes = self.take(size)?;
        Ok(bytes.iter().fold(0u64, |v, b| (v << 8) | u64::from(*b)))
    }
    fn object(&mut self) -> Result<u64, String> {
        self.id(self.sizes.object)
    }
    fn reference(&mut self) -> Result<u64, String> {
        self.id(self.sizes.reference)
    }
    fn method(&mut self) -> Result<u64, String> {
        self.id(self.sizes.method)
    }
    fn field(&mut self) -> Result<u64, String> {
        self.id(self.sizes.field)
    }
    fn frame(&mut self) -> Result<u64, String> {
        self.id(self.sizes.frame)
    }
    fn count(&mut self) -> Result<usize, String> {
        let n = self.i32()?;
        usize::try_from(n).map_err(|_| "a JDWP count was negative".to_string())
    }
    fn string(&mut self) -> Result<String, String> {
        let n = self.count()?;
        Ok(String::from_utf8_lossy(self.take(n)?).into_owned())
    }
    fn location(&mut self) -> Result<Location, String> {
        Ok(Location {
            tag: self.u8()?,
            class: self.reference()?,
            method: self.method()?,
            index: self.u64()?,
        })
    }
    /// A value of type `tag`, untagged.
    fn untagged(&mut self, tag: u8) -> Result<JValue, String> {
        let bits = match tag {
            b'B' | b'Z' => u64::from(self.u8()?),
            b'C' | b'S' => {
                let b = self.take(2)?;
                u64::from(u16::from_be_bytes([b[0], b[1]]))
            }
            b'I' | b'F' => u64::from(self.i32()? as u32),
            b'J' | b'D' => self.u64()?,
            b'V' => 0,
            _ => self.object()?,
        };
        Ok(JValue { tag, bits })
    }
    fn tagged(&mut self) -> Result<JValue, String> {
        let tag = self.u8()?;
        self.untagged(tag)
    }
}

/// An event from the VM.
#[derive(Debug, Clone)]
enum Event {
    Started,
    Stopped { thread: u64, step: bool },
    ClassPrepared { class: u64, signature: String },
    Died,
}

/// The commands waiting for their replies, by packet id.
type Pending = Arc<Mutex<HashMap<u32, oneshot::Sender<Result<Vec<u8>, String>>>>>;

/// The connection to the VM's agent: commands out, replies routed by id,
/// events sent on.
struct Jdwp {
    write: Mutex<tokio::net::tcp::OwnedWriteHalf>,
    pending: Pending,
    next: AtomicU32,
    sizes: std::sync::Mutex<IdSizes>,
    /// The VM is gone: a command fails at once rather than waiting.
    closed: Arc<AtomicBool>,
    /// Objects an evaluation made (a string, a method's result), kept from
    /// the collector while the program is stopped: nothing else refers to
    /// them.
    held: std::sync::Mutex<Vec<u64>>,
    /// The thread running a method for an evaluation (0: none): a
    /// breakpoint it meets is passed, as it is not the program's own run.
    invoking: AtomicU64,
}

impl Jdwp {
    /// Connect (the handshake done) and read packets: replies to their
    /// waiting command, events to `events`.
    async fn connect(
        addr: std::net::SocketAddr,
        events: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    ) -> Result<Arc<Jdwp>, String> {
        let mut stream = tokio::net::TcpStream::connect(addr)
            .await
            .map_err(|e| format!("cannot reach the JVM's agent: {e}"))?;
        stream
            .write_all(b"JDWP-Handshake")
            .await
            .map_err(|e| e.to_string())?;
        let mut answer = [0u8; 14];
        stream
            .read_exact(&mut answer)
            .await
            .map_err(|e| format!("the JVM's agent did not shake hands: {e}"))?;
        if &answer != b"JDWP-Handshake" {
            return Err("the JVM's agent answered something else".into());
        }
        let (mut read, write) = stream.into_split();
        let pending: Pending = Arc::default();
        let closed = Arc::new(AtomicBool::new(false));
        let jdwp = Arc::new(Jdwp {
            write: Mutex::new(write),
            pending: pending.clone(),
            next: AtomicU32::new(1),
            sizes: std::sync::Mutex::new(IdSizes::default()),
            closed: closed.clone(),
            held: std::sync::Mutex::default(),
            invoking: AtomicU64::new(0),
        });
        tokio::spawn(async move {
            loop {
                let mut head = [0u8; 11];
                if read.read_exact(&mut head).await.is_err() {
                    break;
                }
                let length = u32::from_be_bytes(head[0..4].try_into().unwrap()) as usize;
                let id = u32::from_be_bytes(head[4..8].try_into().unwrap());
                let flags = head[8];
                if !(11..=MAX_PACKET).contains(&length) {
                    break;
                }
                let mut data = vec![0u8; length - 11];
                if read.read_exact(&mut data).await.is_err() {
                    break;
                }
                if flags & 0x80 != 0 {
                    let error = u16::from_be_bytes([head[9], head[10]]);
                    if let Some(waiting) = pending.lock().await.remove(&id) {
                        let _ = waiting.send(match error {
                            0 => Ok(data),
                            code => Err(jdwp_error(code)),
                        });
                    }
                } else if head[9] == 64 && head[10] == 100 {
                    let _ = events.send(data);
                }
            }
            // The VM is gone: whoever waits hears so, and whoever asks next.
            let mut pending = pending.lock().await;
            closed.store(true, Ordering::SeqCst);
            pending.clear();
        });
        Ok(jdwp)
    }

    fn sizes(&self) -> IdSizes {
        *self.sizes.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn data(&self) -> Writer {
        Writer::new(self.sizes())
    }

    /// Send command `set`/`command` with `data`, and wait for its reply.
    async fn call(&self, set: u8, command: u8, data: Writer) -> Result<Vec<u8>, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            if self.closed.load(Ordering::SeqCst) {
                return Err("the JVM is gone".into());
            }
            pending.insert(id, tx);
        }
        let mut packet = Vec::with_capacity(11 + data.bytes.len());
        packet.extend_from_slice(&((11 + data.bytes.len()) as u32).to_be_bytes());
        packet.extend_from_slice(&id.to_be_bytes());
        packet.extend_from_slice(&[0, set, command]);
        packet.extend_from_slice(&data.bytes);
        self.write
            .lock()
            .await
            .write_all(&packet)
            .await
            .map_err(|e| format!("the JVM is gone: {e}"))?;
        match tokio::time::timeout(ANSWER_TIMEOUT, rx).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => Err("the JVM is gone".into()),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err("the JVM did not answer in time".into())
            }
        }
    }

    fn reader<'a>(&self, bytes: &'a [u8]) -> Reader<'a> {
        Reader::new(bytes, self.sizes())
    }

    async fn id_sizes(&self) -> Result<(), String> {
        let reply = self.call(1, 7, Writer::default()).await?;
        let mut r = Reader::new(&reply, IdSizes::default());
        let sizes = IdSizes {
            field: r.count()?,
            method: r.count()?,
            object: r.count()?,
            reference: r.count()?,
            frame: r.count()?,
        };
        *self.sizes.lock().unwrap_or_else(|e| e.into_inner()) = sizes;
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        self.call(1, 9, self.data()).await.map(|_| ())
    }

    /// Keep `value` (an object an evaluation made) from the collector until
    /// [`Jdwp::release`].
    async fn hold(&self, value: JValue) -> JValue {
        if !is_primitive(value.tag) && value.tag != b'V' && value.bits != 0 {
            let held = self.call(9, 7, self.data().object(value.bits)).await;
            if held.is_ok() {
                self.held
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(value.bits);
            }
        }
        value
    }

    /// Let the collector have what evaluations made: the program runs on.
    async fn release(&self) {
        let held = std::mem::take(&mut *self.held.lock().unwrap_or_else(|e| e.into_inner()));
        for object in held {
            let _ = self.call(9, 8, self.data().object(object)).await;
        }
    }

    async fn suspend(&self) -> Result<(), String> {
        self.call(1, 8, self.data()).await.map(|_| ())
    }

    /// The loaded classes of this signature (`Lpkg/Main;`).
    async fn classes_by_signature(&self, signature: &str) -> Result<Vec<u64>, String> {
        let reply = self.call(1, 2, self.data().string(signature)).await?;
        let mut r = self.reader(&reply);
        let n = r.count()?;
        let mut classes = Vec::new();
        for _ in 0..n {
            r.u8()?;
            classes.push(r.reference()?);
            r.i32()?;
        }
        Ok(classes)
    }

    async fn signature(&self, class: u64) -> Result<String, String> {
        let reply = self.call(2, 1, self.data().reference(class)).await?;
        self.reader(&reply).string()
    }

    async fn source_file(&self, class: u64) -> Result<String, String> {
        let reply = self.call(2, 7, self.data().reference(class)).await?;
        self.reader(&reply).string()
    }

    /// A class's methods: id, name, signature, modifiers.
    async fn methods(&self, class: u64) -> Result<Vec<(u64, String, String, i32)>, String> {
        let reply = self.call(2, 5, self.data().reference(class)).await?;
        let mut r = self.reader(&reply);
        let n = r.count()?;
        let mut methods = Vec::new();
        for _ in 0..n {
            methods.push((r.method()?, r.string()?, r.string()?, r.i32()?));
        }
        Ok(methods)
    }

    /// A class's own fields: id, name, signature, modifiers.
    async fn fields(&self, class: u64) -> Result<Vec<(u64, String, String, i32)>, String> {
        let reply = self.call(2, 4, self.data().reference(class)).await?;
        let mut r = self.reader(&reply);
        let n = r.count()?;
        let mut fields = Vec::new();
        for _ in 0..n {
            fields.push((r.field()?, r.string()?, r.string()?, r.i32()?));
        }
        Ok(fields)
    }

    /// The interfaces a type declares it implements (or extends).
    async fn interfaces(&self, class: u64) -> Result<Vec<u64>, String> {
        let reply = self.call(2, 10, self.data().reference(class)).await?;
        let mut r = self.reader(&reply);
        let n = r.count()?;
        (0..n).map(|_| r.reference()).collect()
    }

    async fn superclass(&self, class: u64) -> Result<u64, String> {
        let reply = self.call(3, 1, self.data().reference(class)).await?;
        self.reader(&reply).reference()
    }

    /// A method's line table: each line with the first index of its code.
    async fn line_table(&self, class: u64, method: u64) -> Result<Vec<(u64, i32)>, String> {
        let reply = self
            .call(6, 1, self.data().reference(class).method(method))
            .await?;
        let mut r = self.reader(&reply);
        r.u64()?;
        r.u64()?;
        let n = r.count()?;
        let mut lines = Vec::new();
        for _ in 0..n {
            lines.push((r.u64()?, r.i32()?));
        }
        Ok(lines)
    }

    /// A method's local variables: where each is valid, its name, signature
    /// and slot.
    async fn variable_table(
        &self,
        class: u64,
        method: u64,
    ) -> Result<Vec<(u64, u32, String, String, i32)>, String> {
        let reply = self
            .call(6, 2, self.data().reference(class).method(method))
            .await?;
        let mut r = self.reader(&reply);
        r.i32()?;
        let n = r.count()?;
        let mut variables = Vec::new();
        for _ in 0..n {
            let start = r.u64()?;
            let name = r.string()?;
            let signature = r.string()?;
            let length = r.i32()? as u32;
            let slot = r.i32()?;
            variables.push((start, length, name, signature, slot));
        }
        Ok(variables)
    }

    async fn all_threads(&self) -> Result<Vec<u64>, String> {
        let reply = self.call(1, 4, self.data()).await?;
        let mut r = self.reader(&reply);
        let n = r.count()?;
        (0..n).map(|_| r.object()).collect()
    }

    async fn thread_name(&self, thread: u64) -> Result<String, String> {
        let reply = self.call(11, 1, self.data().object(thread)).await?;
        self.reader(&reply).string()
    }

    /// A thread's frames, top first.
    async fn frames(&self, thread: u64) -> Result<Vec<(u64, Location)>, String> {
        let reply = self
            .call(11, 6, self.data().object(thread).i32(0).i32(-1))
            .await?;
        let mut r = self.reader(&reply);
        let n = r.count()?;
        let mut frames = Vec::new();
        for _ in 0..n {
            frames.push((r.frame()?, r.location()?));
        }
        Ok(frames)
    }

    async fn frame_values(
        &self,
        thread: u64,
        frame: u64,
        slots: &[(i32, u8)],
    ) -> Result<Vec<JValue>, String> {
        let mut data = self
            .data()
            .object(thread)
            .frame(frame)
            .i32(slots.len() as i32);
        for (slot, tag) in slots {
            data = data.i32(*slot).u8(*tag);
        }
        let reply = self.call(16, 1, data).await?;
        let mut r = self.reader(&reply);
        let n = r.count()?;
        (0..n).map(|_| r.tagged()).collect()
    }

    async fn this_object(&self, thread: u64, frame: u64) -> Result<JValue, String> {
        let reply = self
            .call(16, 3, self.data().object(thread).frame(frame))
            .await?;
        self.reader(&reply).tagged()
    }

    async fn object_class(&self, object: u64) -> Result<u64, String> {
        let reply = self.call(9, 1, self.data().object(object)).await?;
        let mut r = self.reader(&reply);
        r.u8()?;
        r.reference()
    }

    async fn object_values(&self, object: u64, fields: &[u64]) -> Result<Vec<JValue>, String> {
        let mut data = self.data().object(object).i32(fields.len() as i32);
        for field in fields {
            data = data.field(*field);
        }
        let reply = self.call(9, 2, data).await?;
        let mut r = self.reader(&reply);
        let n = r.count()?;
        (0..n).map(|_| r.tagged()).collect()
    }

    async fn string_value(&self, object: u64) -> Result<String, String> {
        let reply = self.call(10, 1, self.data().object(object)).await?;
        self.reader(&reply).string()
    }

    async fn array_length(&self, array: u64) -> Result<usize, String> {
        let reply = self.call(13, 1, self.data().object(array)).await?;
        self.reader(&reply).count()
    }

    async fn array_values(
        &self,
        array: u64,
        first: usize,
        length: usize,
    ) -> Result<Vec<JValue>, String> {
        let reply = self
            .call(
                13,
                2,
                self.data()
                    .object(array)
                    .i32(first as i32)
                    .i32(length as i32),
            )
            .await?;
        let mut r = self.reader(&reply);
        let tag = r.u8()?;
        let n = r.count()?;
        let primitive = matches!(tag, b'B' | b'C' | b'F' | b'D' | b'I' | b'J' | b'S' | b'Z');
        (0..n)
            .map(|_| match primitive {
                true => r.untagged(tag),
                false => r.tagged(),
            })
            .collect()
    }

    /// Static fields' values (`ReferenceType.GetValues`).
    async fn static_values(&self, class: u64, fields: &[u64]) -> Result<Vec<JValue>, String> {
        let mut data = self.data().reference(class).i32(fields.len() as i32);
        for field in fields {
            data = data.field(*field);
        }
        let reply = self.call(2, 6, data).await?;
        let mut r = self.reader(&reply);
        let n = r.count()?;
        (0..n).map(|_| r.tagged()).collect()
    }

    /// A new string in the VM.
    async fn create_string(&self, text: &str) -> Result<JValue, String> {
        let reply = self.call(1, 11, self.data().string(text)).await?;
        let string = JValue {
            tag: b's',
            bits: self.reader(&reply).object()?,
        };
        Ok(self.hold(string).await)
    }

    /// Invoke a method in `thread` — on `object`, or a static one of
    /// `class` when `object` is none — the other threads left suspended.
    async fn invoke(
        &self,
        object: Option<u64>,
        class: u64,
        thread: u64,
        method: u64,
        args: &[JValue],
    ) -> Result<JValue, String> {
        let mut data = match object {
            Some(object) => self.data().object(object).object(thread).reference(class),
            None => self.data().reference(class).object(thread),
        }
        .method(method)
        .i32(args.len() as i32);
        for arg in args {
            data = data.u8(arg.tag);
            data = match arg.tag {
                b'B' | b'Z' => data.u8(arg.bits as u8),
                b'C' | b'S' => {
                    data.bytes
                        .extend_from_slice(&(arg.bits as u16).to_be_bytes());
                    data
                }
                b'I' | b'F' => data.i32(arg.bits as u32 as i32),
                b'J' | b'D' => data.u64(arg.bits),
                _ => data.object(arg.bits),
            };
        }
        // INVOKE_SINGLE_THREADED: only this thread runs for it.
        let data = data.i32(1);
        self.invoking.store(thread, Ordering::SeqCst);
        let reply = match object {
            Some(_) => self.call(9, 6, data).await,
            None => self.call(3, 3, data).await,
        };
        self.invoking.store(0, Ordering::SeqCst);
        // A thread runs a method only from where an event stopped it — not
        // from a pause.
        let reply = reply.map_err(|e| match e.as_str() {
            "no such thread" => {
                "methods are called only in a thread stopped at a breakpoint or a step".to_string()
            }
            _ => e,
        })?;
        let mut r = self.reader(&reply);
        let value = r.tagged()?;
        let exception = r.tagged()?;
        if exception.bits != 0 {
            let class = self.object_class(exception.bits).await.unwrap_or(0);
            let name = class_name(&self.signature(class).await.unwrap_or_default());
            return Err(format!("it threw {name}"));
        }
        Ok(self.hold(value).await)
    }

    /// Ask for events of `kind`, the VM suspended when one comes.
    async fn request(&self, kind: u8, modifiers: Vec<Writer>) -> Result<i32, String> {
        let mut data = self.data().u8(kind).u8(2).i32(modifiers.len() as i32);
        for modifier in modifiers {
            data.bytes.extend_from_slice(&modifier.bytes);
        }
        let reply = self.call(15, 1, data).await?;
        self.reader(&reply).i32()
    }

    async fn clear(&self, kind: u8, request: i32) -> Result<(), String> {
        self.call(15, 2, self.data().u8(kind).i32(request))
            .await
            .map(|_| ())
    }

    async fn dispose(&self) {
        let _ = self.call(1, 6, self.data()).await;
    }

    /// One composite event packet's events.
    fn events(&self, data: &[u8]) -> Result<Vec<Event>, String> {
        let mut r = self.reader(data);
        r.u8()?;
        let n = r.count()?;
        let mut events = Vec::new();
        for _ in 0..n {
            let kind = r.u8()?;
            r.i32()?;
            match kind {
                90 => {
                    r.object()?;
                    events.push(Event::Started);
                }
                1 | 2 => {
                    let thread = r.object()?;
                    r.location()?;
                    events.push(Event::Stopped {
                        thread,
                        step: kind == 1,
                    });
                }
                8 => {
                    r.object()?;
                    r.u8()?;
                    let class = r.reference()?;
                    let signature = r.string()?;
                    r.i32()?;
                    events.push(Event::ClassPrepared { class, signature });
                }
                6 | 7 => {
                    r.object()?;
                }
                99 => events.push(Event::Died),
                // Kinds never asked for: the rest of the packet cannot be
                // read past them.
                _ => break,
            }
        }
        Ok(events)
    }
}

fn jdwp_error(code: u16) -> String {
    match code {
        10 => "no such thread".into(),
        13 => "the thread is not suspended".into(),
        21 => "the class is not loaded".into(),
        30 => "no such frame".into(),
        35 => "invalid slot".into(),
        41 => "an object was collected".into(),
        101 => "absent information — compile with -g".into(),
        112 => "the VM is gone".into(),
        other => format!("JDWP error {other}"),
    }
}

// -- the adapter -------------------------------------------------------------

/// A breakpoint asked for: by file, its line; set once its class loads.
#[derive(Debug, Clone)]
struct Wanted {
    id: u64,
    line: i32,
    /// The JDWP request, once set.
    request: Option<i32>,
}

/// What a `variablesReference` stands for.
#[derive(Debug, Clone, Copy)]
enum VarRef {
    /// A frame's locals (and `this`), the frame by its depth: a frame's id
    /// does not outlive the thread running (a method invoked in it).
    Locals {
        thread: u64,
        depth: usize,
        location: Location,
    },
    /// An object's fields or an array's elements.
    Object { object: u64, array: bool },
}

#[derive(Default)]
struct State {
    /// Each source file's breakpoints, and the classes they are for.
    breakpoints: HashMap<String, (String, Vec<Wanted>)>,
    /// The DAP frame ids handed out since the program last stopped: each
    /// frame's thread, depth and location.
    frames: Vec<(u64, usize, Location)>,
    refs: Vec<VarRef>,
    /// The step request in effect, cleared when it ends.
    step: Option<i32>,
}

struct Process {
    exited: watch::Receiver<Option<i32>>,
    kill: Option<oneshot::Sender<()>>,
}

struct Adapter {
    java: PathBuf,
    javac: PathBuf,
    out: Out,
    vm: Option<Arc<Jdwp>>,
    process: Option<Process>,
    scratch: Option<PathBuf>,
    /// Where source files are, by their package's path.
    roots: Vec<PathBuf>,
    state: Arc<Mutex<State>>,
}

static NEXT_BREAKPOINT: AtomicU64 = AtomicU64::new(1);

impl Adapter {
    async fn handle(&mut self, request: &Value) -> bool {
        let command = request["command"].as_str().unwrap_or("");
        let args = &request["arguments"];
        let answer = match command {
            "initialize" => Ok(json!({
                "supportsConfigurationDoneRequest": true,
                "supportsEvaluateForHovers": true,
                "supportsTerminateRequest": true,
            })),
            "launch" => match self.launch(args).await {
                Ok(()) => {
                    self.out.respond(request, Ok(json!({}))).await;
                    self.out.event("initialized", json!({})).await;
                    return true;
                }
                Err(e) => Err(e),
            },
            "setBreakpoints" => self.set_breakpoints(args).await,
            // The program was held at its start; it runs now.
            "configurationDone" => match self.vm() {
                Ok(vm) => vm.resume().await.map(|()| json!({})),
                Err(e) => Err(e),
            },
            "threads" => self.threads().await,
            "continue" => match self.vm() {
                Ok(vm) => {
                    vm.release().await;
                    vm.resume()
                        .await
                        .map(|()| json!({ "allThreadsContinued": true }))
                }
                Err(e) => Err(e),
            },
            "next" | "stepIn" | "stepOut" => {
                let depth = match command {
                    "stepIn" => 0,
                    "next" => 1,
                    _ => 2,
                };
                self.step(args, depth).await
            }
            "pause" => match self.vm() {
                Ok(vm) => match vm.suspend().await {
                    Ok(()) => {
                        self.out.respond(request, Ok(json!({}))).await;
                        let thread = args["threadId"].as_u64().unwrap_or(0);
                        self.forget_frames().await;
                        self.out
                            .event("stopped", json!({ "reason": "pause", "threadId": thread, "allThreadsStopped": true }))
                            .await;
                        return true;
                    }
                    Err(e) => Err(e),
                },
                Err(e) => Err(e),
            },
            "stackTrace" => self.stack_trace(args).await,
            "scopes" => self.scopes(args).await,
            "variables" => self.variables(args).await,
            "evaluate" => self.evaluate(args).await,
            "disconnect" | "terminate" => {
                self.out.respond(request, Ok(json!({}))).await;
                return false;
            }
            other => Err(format!("{other} is not supported")),
        };
        self.out.respond(request, answer).await;
        true
    }

    fn vm(&self) -> Result<Arc<Jdwp>, String> {
        self.vm
            .clone()
            .ok_or_else(|| "no program is running".to_string())
    }

    async fn forget_frames(&self) {
        let mut state = self.state.lock().await;
        state.frames.clear();
        state.refs.clear();
    }

    /// Compile the program when it is a source file, run it suspended under
    /// the agent, and connect.
    async fn launch(&mut self, args: &Value) -> Result<(), String> {
        let program = args["program"]
            .as_str()
            .filter(|p| !p.is_empty())
            .map(PathBuf::from);
        let cwd = args["cwd"]
            .as_str()
            .map(PathBuf::from)
            .or_else(|| {
                program
                    .as_ref()
                    .and_then(|p| p.parent().map(Path::to_path_buf))
            })
            .unwrap_or_else(|| PathBuf::from("."));
        let mut classpath: Vec<PathBuf> = args["classPaths"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|p| cwd.join(p)))
                    .collect()
            })
            .unwrap_or_default();
        classpath.extend(build_outputs(&cwd));
        self.roots = source_roots(&cwd);
        let main = match (args["mainClass"].as_str(), &program) {
            (Some(main), _) => main.to_string(),
            (None, Some(program)) if program.extension().is_some_and(|e| e == "java") => {
                let source = std::fs::read_to_string(program)
                    .map_err(|e| format!("cannot read {}: {e}", program.display()))?;
                let package = package_of(&source);
                let stem = program
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .ok_or("no class to run")?;
                let root = source_root(program, package.as_deref());
                if !self.roots.contains(&root) {
                    self.roots.insert(0, root.clone());
                }
                let scratch = scratch_dir()?;
                self.scratch = Some(scratch.clone());
                self.compile(program, &root, &classpath, &scratch).await?;
                classpath.insert(0, scratch);
                match package {
                    Some(package) => format!("{package}.{stem}"),
                    None => stem,
                }
            }
            _ => return Err("launch a .java file, or name the mainClass to run".into()),
        };
        let program_args: Vec<String> = args["args"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let vm_args: Vec<String> = args["vmArgs"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let joined = std::env::join_paths(&classpath).map_err(|e| e.to_string())?;
        let mut command = clew_core::spawn::tokio_command(&self.java);
        command
            .arg("-agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address=127.0.0.1:0")
            .args(&vm_args)
            .arg("-cp")
            .arg(&joined)
            .arg(&main)
            .args(&program_args)
            .current_dir(&cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|e| format!("cannot run {}: {e}", self.java.display()))?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(forward(Box::new(stderr), "stderr", self.out.clone()));
        }
        let (exit_tx, mut exited) = watch::channel(None);
        let (kill, killed) = oneshot::channel::<()>();
        let out = self.out.clone();
        tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => status.ok(),
                _ = killed => {
                    let _ = child.kill().await;
                    child.wait().await.ok()
                }
            };
            let code = status.and_then(|s| s.code()).unwrap_or(-1);
            let _ = exit_tx.send(Some(code));
            out.terminated(Some(code)).await;
        });
        self.process = Some(Process {
            exited: exited.clone(),
            kill: Some(kill),
        });
        // The agent says where it listens, on stdout, before the program
        // prints anything; the rest is the program's.
        let mut stdout = BufReader::new(stdout);
        let port = tokio::select! {
            found = tokio::time::timeout(START_TIMEOUT, listening_port(&mut stdout)) => match found {
                Ok(Ok(port)) => port,
                Ok(Err(e)) => return Err(e),
                Err(_) => return Err("the JVM did not say where its debugger listens".into()),
            },
            _ = exited.wait_for(Option::is_some) => {
                return Err("the JVM ended before it started — see its output".into());
            }
        };
        tokio::spawn(forward(Box::new(stdout), "stdout", self.out.clone()));
        let (events_tx, events) = tokio::sync::mpsc::unbounded_channel();
        let vm = Jdwp::connect(
            std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            events_tx,
        )
        .await?;
        vm.id_sizes().await?;
        self.vm = Some(vm.clone());
        tokio::spawn(event_loop(vm, events, self.state.clone(), self.out.clone()));
        Ok(())
    }

    async fn compile(
        &self,
        program: &Path,
        root: &Path,
        classpath: &[PathBuf],
        into: &Path,
    ) -> Result<(), String> {
        let mut command = clew_core::spawn::tokio_command(&self.javac);
        command
            .arg("-g")
            .arg("-d")
            .arg(into)
            .arg("-sourcepath")
            .arg(root);
        if !classpath.is_empty() {
            command
                .arg("-cp")
                .arg(std::env::join_paths(classpath).map_err(|e| e.to_string())?);
        }
        command
            .arg(program)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let output = tokio::time::timeout(COMPILE_TIMEOUT, command.output())
            .await
            .map_err(|_| "compiling took too long".to_string())?
            .map_err(|e| format!("cannot run {}: {e}", self.javac.display()))?;
        let said: String = String::from_utf8_lossy(&output.stderr)
            .lines()
            .filter(|l| !l.starts_with("Picked up JAVA_TOOL_OPTIONS"))
            .map(|l| format!("{l}\n"))
            .collect();
        if !said.trim().is_empty() {
            self.out.output("console", said).await;
        }
        match output.status.success() {
            true => Ok(()),
            false => Err("the program did not compile — see the output".into()),
        }
    }

    /// Replace one file's breakpoints: set now on its classes that are
    /// loaded, and when the others load.
    async fn set_breakpoints(&self, args: &Value) -> Result<Value, String> {
        let path = args["source"]["path"]
            .as_str()
            .ok_or("a breakpoint needs its file's path")?
            .to_string();
        let lines: Vec<i32> = args["breakpoints"]
            .as_array()
            .map(|bps| {
                bps.iter()
                    .filter_map(|bp| bp["line"].as_i64().map(|l| l as i32))
                    .collect()
            })
            .unwrap_or_default();
        let vm = self.vm()?;
        let class = std::fs::read_to_string(&path)
            .ok()
            .and_then(|source| {
                let stem = Path::new(&path).file_stem()?.to_string_lossy().into_owned();
                Some(match package_of(&source) {
                    Some(package) => format!("{package}.{stem}"),
                    None => stem,
                })
            })
            .ok_or("not a Java source file")?;
        let mut state = self.state.lock().await;
        if let Some((_, old)) = state.breakpoints.remove(&path) {
            for wanted in old {
                if let Some(request) = wanted.request {
                    let _ = vm.clear(2, request).await;
                }
            }
        }
        let mut wanted: Vec<Wanted> = lines
            .iter()
            .map(|&line| Wanted {
                id: NEXT_BREAKPOINT.fetch_add(1, Ordering::Relaxed),
                line,
                request: None,
            })
            .collect();
        // The class and its nested ones, loaded now…
        let signature = format!("L{};", class.replace('.', "/"));
        for loaded in vm
            .classes_by_signature(&signature)
            .await
            .unwrap_or_default()
        {
            place(&vm, loaded, &mut wanted).await;
        }
        // …and when they load.
        for pattern in [class.clone(), format!("{class}$*")] {
            let _ = vm.request(8, vec![vm.data().u8(5).string(&pattern)]).await;
        }
        let answers: Vec<Value> = wanted
            .iter()
            .map(|w| match w.request {
                Some(_) => json!({ "id": w.id, "verified": true, "line": w.line }),
                None => json!({
                    "id": w.id, "verified": false, "line": w.line,
                    "message": "set when its class loads",
                }),
            })
            .collect();
        state.breakpoints.insert(path, (class, wanted));
        Ok(json!({ "breakpoints": answers }))
    }

    async fn threads(&self) -> Result<Value, String> {
        let Ok(vm) = self.vm() else {
            return Ok(json!({ "threads": [] }));
        };
        let mut threads = Vec::new();
        for thread in vm.all_threads().await? {
            let name = vm.thread_name(thread).await.unwrap_or_default();
            threads.push(json!({ "id": thread, "name": name }));
        }
        Ok(json!({ "threads": threads }))
    }

    async fn step(&self, args: &Value, depth: i32) -> Result<Value, String> {
        let vm = self.vm()?;
        let thread = args["threadId"].as_u64().ok_or("which thread?")?;
        let mut modifiers = vec![vm.data().u8(10).object(thread).i32(1).i32(depth)];
        for excluded in NOT_STEPPED_INTO {
            modifiers.push(vm.data().u8(6).string(excluded));
        }
        modifiers.push(vm.data().u8(1).i32(1));
        let request = vm.request(1, modifiers).await?;
        self.state.lock().await.step = Some(request);
        self.forget_frames().await;
        vm.release().await;
        vm.resume().await?;
        Ok(json!({}))
    }

    async fn stack_trace(&self, args: &Value) -> Result<Value, String> {
        let vm = self.vm()?;
        let thread = args["threadId"].as_u64().ok_or("which thread?")?;
        let frames = vm.frames(thread).await?;
        let mut shown = Vec::new();
        for (depth, (_, location)) in frames.into_iter().enumerate() {
            let id = {
                let mut state = self.state.lock().await;
                state.frames.push((thread, depth, location));
                state.frames.len()
            };
            let signature = vm.signature(location.class).await.unwrap_or_default();
            let class = class_name(&signature);
            let method = vm
                .methods(location.class)
                .await
                .unwrap_or_default()
                .into_iter()
                .find(|m| m.0 == location.method)
                .map(|m| m.1)
                .unwrap_or_else(|| "?".into());
            let line = vm
                .line_table(location.class, location.method)
                .await
                .ok()
                .and_then(|table| line_at(&table, location.index))
                .unwrap_or(0);
            let mut frame = json!({
                "id": id,
                "name": format!("{}.{method}", class.rsplit('.').next().unwrap_or(&class)),
                "line": line,
                "column": 1,
            });
            if let Some(path) = self.source_of(&vm, location.class, &signature).await {
                frame["source"] = json!({
                    "path": path.to_string_lossy(),
                    "name": path.file_name().map(|n| n.to_string_lossy().into_owned()),
                });
            } else {
                frame["presentationHint"] = json!("subtle");
            }
            shown.push(frame);
        }
        Ok(json!({ "totalFrames": shown.len(), "stackFrames": shown }))
    }

    /// The source file of a class, found under the source roots by its
    /// package and its `SourceFile`.
    async fn source_of(&self, vm: &Jdwp, class: u64, signature: &str) -> Option<PathBuf> {
        let file = vm.source_file(class).await.ok()?;
        let name = class_name(signature);
        let package = name.rsplit_once('.').map(|(p, _)| p.replace('.', "/"));
        self.roots.iter().find_map(|root| {
            let path = match &package {
                Some(package) => root.join(package).join(&file),
                None => root.join(&file),
            };
            path.is_file().then_some(path)
        })
    }

    async fn scopes(&self, args: &Value) -> Result<Value, String> {
        let id = args["frameId"].as_u64().unwrap_or(0) as usize;
        let mut state = self.state.lock().await;
        let (thread, depth, location) = *id
            .checked_sub(1)
            .and_then(|i| state.frames.get(i))
            .ok_or("no such frame")?;
        state.refs.push(VarRef::Locals {
            thread,
            depth,
            location,
        });
        Ok(
            json!({ "scopes": [{ "name": "Locals", "variablesReference": state.refs.len(), "expensive": false }] }),
        )
    }

    async fn variables(&self, args: &Value) -> Result<Value, String> {
        let reference = args["variablesReference"].as_u64().unwrap_or(0) as usize;
        let found = {
            let state = self.state.lock().await;
            reference
                .checked_sub(1)
                .and_then(|i| state.refs.get(i))
                .copied()
        };
        let Some(var) = found else {
            return Ok(json!({ "variables": [] }));
        };
        let vm = self.vm()?;
        let named = match var {
            VarRef::Locals {
                thread,
                depth,
                location,
            } => locals(&vm, thread, frame_id(&vm, thread, depth).await?, location).await?,
            VarRef::Object { object, array } => children(&vm, object, array).await?,
        };
        let mut variables = Vec::new();
        for (name, value) in named {
            variables.push(self.shown(&vm, &name, value).await);
        }
        Ok(json!({ "variables": variables }))
    }

    /// A value as a DAP variable: an object or array given a reference to
    /// open it by.
    async fn shown(&self, vm: &Jdwp, name: &str, value: JValue) -> Value {
        let text = display(vm, value).await;
        let reference = match value.tag {
            b'L' | b'[' if value.bits != 0 => {
                let mut state = self.state.lock().await;
                state.refs.push(VarRef::Object {
                    object: value.bits,
                    array: value.tag == b'[',
                });
                state.refs.len()
            }
            _ => 0,
        };
        json!({ "name": name, "value": text, "variablesReference": reference })
    }

    /// An expression, evaluated in a frame: JDWP evaluates none itself, so
    /// it is parsed here ([`parse_expression`]) and worked out over JDWP —
    /// variables and fields read, methods invoked in the stopped thread.
    async fn evaluate(&self, args: &Value) -> Result<Value, String> {
        let expression = args["expression"].as_str().unwrap_or("").trim().to_string();
        let parsed = parse_expression(&expression)?;
        let id = args["frameId"].as_u64().unwrap_or(0) as usize;
        let (thread, depth, location) = {
            let state = self.state.lock().await;
            *id.checked_sub(1)
                .and_then(|i| state.frames.get(i))
                .ok_or("evaluate in a stopped thread's frame")?
        };
        let vm = self.vm()?;
        let frame = Frame {
            vm: &vm,
            thread,
            depth,
            location,
            // Resting the pointer on code runs none of it.
            calls: args["context"] != "hover",
        };
        let value = frame.eval(&parsed).await?;
        let value = match value {
            Val::Text(text) => {
                return Ok(json!({ "result": format!("{text:?}"), "variablesReference": 0 }));
            }
            Val::J(value) => value,
        };
        let shown = self.shown(&vm, &expression, value).await;
        Ok(json!({ "result": shown["value"], "variablesReference": shown["variablesReference"] }))
    }

    async fn end(&mut self) {
        if let Some(vm) = self.vm.take() {
            vm.dispose().await;
        }
        if let Some(process) = &mut self.process {
            if let Some(kill) = process.kill.take() {
                let _ = kill.send(());
            }
            let mut exited = process.exited.clone();
            let _ = tokio::time::timeout(EXIT_GRACE, exited.wait_for(Option::is_some)).await;
        }
        if let Some(scratch) = self.scratch.take() {
            let _ = std::fs::remove_dir_all(scratch);
        }
    }
}

/// Set `wanted`'s breakpoints that fall in `class`: each at the first code
/// of its line, in whichever method has it.
async fn place(vm: &Jdwp, class: u64, wanted: &mut [Wanted]) -> Vec<u64> {
    let mut placed = Vec::new();
    let methods = vm.methods(class).await.unwrap_or_default();
    let mut tables = Vec::new();
    for (method, ..) in &methods {
        if let Ok(table) = vm.line_table(class, *method).await {
            tables.push((*method, table));
        }
    }
    for w in wanted.iter_mut().filter(|w| w.request.is_none()) {
        let at = tables.iter().find_map(|(method, table)| {
            table
                .iter()
                .filter(|(_, line)| *line == w.line)
                .map(|(index, _)| *index)
                .min()
                .map(|index| (*method, index))
        });
        if let Some((method, index)) = at {
            let location = Location {
                tag: 1,
                class,
                method,
                index,
            };
            if let Ok(request) = vm
                .request(2, vec![vm.data().u8(7).location(location)])
                .await
            {
                w.request = Some(request);
                placed.push(w.id);
            }
        }
    }
    placed
}

/// Read the VM's events: a stop is reported, a class that loads gets its
/// breakpoints (and the program goes on), the VM's end is the program's.
async fn event_loop(
    vm: Arc<Jdwp>,
    mut events: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    state: Arc<Mutex<State>>,
    out: Out,
) {
    let started = AtomicBool::new(false);
    while let Some(packet) = events.recv().await {
        let Ok(parsed) = vm.events(&packet) else {
            continue;
        };
        let mut resume = false;
        for event in parsed {
            match event {
                Event::Started => {
                    // Held until the session is configured.
                    started.store(true, Ordering::SeqCst);
                }
                Event::ClassPrepared { class, signature } => {
                    let name = class_name(&signature);
                    let outer = name.split('$').next().unwrap_or(&name).to_string();
                    let mut state = state.lock().await;
                    for (for_class, wanted) in state.breakpoints.values_mut() {
                        if *for_class != outer {
                            continue;
                        }
                        for id in place(&vm, class, wanted).await {
                            if let Some(w) = wanted.iter().find(|w| w.id == id) {
                                out.event(
                                    "breakpoint",
                                    json!({ "reason": "changed", "breakpoint": { "id": id, "verified": true, "line": w.line } }),
                                )
                                .await;
                            }
                        }
                    }
                    resume = true;
                }
                // A breakpoint in a method an evaluation called: passed.
                Event::Stopped {
                    thread,
                    step: false,
                } if vm.invoking.load(Ordering::SeqCst) == thread => {
                    resume = true;
                }
                Event::Stopped { thread, step } => {
                    let mut state = state.lock().await;
                    if step && let Some(request) = state.step.take() {
                        let _ = vm.clear(1, request).await;
                    }
                    state.frames.clear();
                    state.refs.clear();
                    drop(state);
                    out.event(
                        "stopped",
                        json!({
                            "reason": if step { "step" } else { "breakpoint" },
                            "threadId": thread,
                            "allThreadsStopped": true,
                        }),
                    )
                    .await;
                }
                Event::Died => {}
            }
        }
        if resume {
            let _ = vm.resume().await;
        }
    }
}

/// A frame's locals visible at its location — `this` first, for an instance
/// method — by name.
async fn locals(
    vm: &Jdwp,
    thread: u64,
    frame: u64,
    location: Location,
) -> Result<Vec<(String, JValue)>, String> {
    let table = vm.variable_table(location.class, location.method).await?;
    let visible: Vec<(String, i32, u8)> = table
        .into_iter()
        .filter(|(start, length, ..)| {
            location.index >= *start && location.index < start + u64::from(*length)
        })
        .map(|(_, _, name, signature, slot)| (name, slot, signature.as_bytes()[0]))
        .collect();
    let mut named = Vec::new();
    if let Ok(this) = vm.this_object(thread, frame).await
        && this.bits != 0
    {
        named.push(("this".to_string(), this));
    }
    let slots: Vec<(i32, u8)> = visible.iter().map(|(_, slot, tag)| (*slot, *tag)).collect();
    if !slots.is_empty() {
        let values = vm.frame_values(thread, frame, &slots).await?;
        for ((name, ..), value) in visible.into_iter().zip(values) {
            named.push((name, value));
        }
    }
    Ok(named)
}

/// An object's fields (its class's and its superclasses'), or an array's
/// first elements.
async fn children(vm: &Jdwp, object: u64, array: bool) -> Result<Vec<(String, JValue)>, String> {
    if array {
        let length = vm.array_length(object).await?.min(MAX_CHILDREN);
        let values = vm.array_values(object, 0, length).await?;
        return Ok(values
            .into_iter()
            .enumerate()
            .map(|(i, v)| (format!("[{i}]"), v))
            .collect());
    }
    let mut class = vm.object_class(object).await?;
    let mut named = Vec::new();
    for _ in 0..16 {
        if class == 0 {
            break;
        }
        let fields: Vec<(u64, String, String, i32)> = vm
            .fields(class)
            .await?
            .into_iter()
            .filter(|f| f.3 & 0x0008 == 0)
            .collect();
        let ids: Vec<u64> = fields.iter().map(|f| f.0).collect();
        if !ids.is_empty() {
            let values = vm.object_values(object, &ids).await?;
            for (field, value) in fields.into_iter().zip(values) {
                named.push((field.1, value));
            }
        }
        if named.len() >= MAX_CHILDREN {
            break;
        }
        class = vm.superclass(class).await.unwrap_or(0);
    }
    named.truncate(MAX_CHILDREN);
    Ok(named)
}

async fn field_of(vm: &Jdwp, value: JValue, name: &str) -> Result<JValue, String> {
    if value.tag == b'[' && name == "length" {
        let length = vm.array_length(value.bits).await?;
        return Ok(JValue {
            tag: b'I',
            bits: length as u64,
        });
    }
    if value.bits == 0 || !matches!(value.tag, b'L' | b's' | b't' | b'g' | b'l' | b'c') {
        return Err(format!("no field {name} there"));
    }
    children(vm, value.bits, false)
        .await?
        .into_iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v)
        .ok_or_else(|| format!("no field {name} there"))
}

async fn element_of(vm: &Jdwp, value: JValue, index: usize) -> Result<JValue, String> {
    if value.tag != b'[' || value.bits == 0 {
        return Err("not an array".into());
    }
    vm.array_values(value.bits, index, 1)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| "no such element".to_string())
}

/// A value as shown: a primitive as Java writes it, a string quoted, an
/// object by its class.
async fn display(vm: &Jdwp, value: JValue) -> String {
    match value.tag {
        b'Z' => (value.bits != 0).to_string(),
        b'B' => (value.bits as u8 as i8).to_string(),
        b'S' => (value.bits as u16 as i16).to_string(),
        b'C' => char::from_u32(value.bits as u32)
            .map(|c| format!("'{c}'"))
            .unwrap_or_default(),
        b'I' => (value.bits as u32 as i32).to_string(),
        b'J' => (value.bits as i64).to_string(),
        b'F' => java_float(f64::from(f32::from_bits(value.bits as u32))),
        b'D' => java_float(f64::from_bits(value.bits)),
        b'V' => "void".into(),
        _ if value.bits == 0 => "null".into(),
        b's' => match vm.string_value(value.bits).await {
            Ok(text) => format!("{text:?}"),
            Err(_) => "\"…\"".into(),
        },
        b'[' => {
            let length = vm.array_length(value.bits).await.unwrap_or(0);
            let class = vm.object_class(value.bits).await.unwrap_or(0);
            let signature = vm.signature(class).await.unwrap_or_default();
            let element = type_name(signature.strip_prefix('[').unwrap_or(&signature));
            format!("{element}[{length}]")
        }
        _ => {
            let class = vm.object_class(value.bits).await.unwrap_or(0);
            let signature = vm.signature(class).await.unwrap_or_default();
            let name = class_name(&signature);
            format!(
                "{}@{:x}",
                name.rsplit('.').next().unwrap_or(&name),
                value.bits
            )
        }
    }
}

/// A floating value as Java writes one: `3.0`, `NaN`, `Infinity`.
fn java_float(v: f64) -> String {
    match v {
        _ if v.is_nan() => "NaN".into(),
        f64::INFINITY => "Infinity".into(),
        f64::NEG_INFINITY => "-Infinity".into(),
        _ => format!("{v:?}"),
    }
}

/// `Lpkg/Main$Inner;` as `pkg.Main$Inner`.
fn class_name(signature: &str) -> String {
    signature
        .strip_prefix('L')
        .and_then(|s| s.strip_suffix(';'))
        .unwrap_or(signature)
        .replace('/', ".")
}

/// A type signature as Java writes the type: `I` as `int`, `[J` as
/// `long[]`, `Ljava/lang/String;` as `String`.
fn type_name(signature: &str) -> String {
    match signature.as_bytes().first() {
        Some(b'Z') => "boolean".into(),
        Some(b'B') => "byte".into(),
        Some(b'C') => "char".into(),
        Some(b'S') => "short".into(),
        Some(b'I') => "int".into(),
        Some(b'J') => "long".into(),
        Some(b'F') => "float".into(),
        Some(b'D') => "double".into(),
        Some(b'[') => format!("{}[]", type_name(&signature[1..])),
        _ => {
            let name = class_name(signature);
            name.rsplit('.').next().unwrap_or(&name).to_string()
        }
    }
}

/// The line a code index is on: the last line whose code starts at or
/// before it.
fn line_at(table: &[(u64, i32)], index: u64) -> Option<i32> {
    table
        .iter()
        .filter(|(start, _)| *start <= index)
        .max_by_key(|(start, _)| *start)
        .map(|(_, line)| *line)
}

// -- expressions --------------------------------------------------------------

/// A Java expression, as much of the language as reads values: literals,
/// names, fields, elements, calls, `this`, and the unary and binary
/// operators on them.
#[derive(Debug, Clone, PartialEq)]
enum Expr {
    Int(i64),
    Long(i64),
    Double(f64),
    Bool(bool),
    Char(char),
    Text(String),
    Null,
    This,
    Name(String),
    Field(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    /// A call: on its receiver, or — none — on `this` or the frame's class.
    Call(Option<Box<Expr>>, String, Vec<Expr>),
    Unary(char, Box<Expr>),
    Binary(&'static str, Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Name(String),
    Int(i64),
    Long(i64),
    Double(f64),
    Text(String),
    Char(char),
    Op(&'static str),
}

const OPERATORS: &[&str] = &[
    "&&", "||", "==", "!=", "<=", ">=", "+", "-", "*", "/", "%", "<", ">", "!", "(", ")", "[", "]",
    ".", ",",
];

fn tokenize(text: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c.is_alphabetic() || c == '_' || c == '$' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || matches!(chars[i], '_' | '$')) {
                i += 1;
            }
            tokens.push(Token::Name(chars[start..i].iter().collect()));
        } else if c.is_ascii_digit() {
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '_')
            {
                i += 1;
            }
            let number: String = chars[start..i].iter().filter(|c| **c != '_').collect();
            let token = if let Some(n) = number.strip_suffix(['L', 'l']) {
                Token::Long(n.parse().map_err(|_| format!("not a number: {number}"))?)
            } else if number.contains(['.', 'e', 'E']) || number.ends_with(['d', 'D', 'f', 'F']) {
                let n = number.trim_end_matches(['d', 'D', 'f', 'F']);
                Token::Double(n.parse().map_err(|_| format!("not a number: {number}"))?)
            } else {
                Token::Int(
                    number
                        .parse()
                        .map_err(|_| format!("not a number: {number}"))?,
                )
            };
            tokens.push(token);
        } else if c == '"' || c == '\'' {
            let mut text = String::new();
            i += 1;
            loop {
                let Some(&ch) = chars.get(i) else {
                    return Err("a quote is not closed".into());
                };
                i += 1;
                match ch {
                    _ if ch == c => break,
                    '\\' => {
                        let escaped = *chars.get(i).ok_or("a quote is not closed")?;
                        i += 1;
                        text.push(match escaped {
                            'n' => '\n',
                            't' => '\t',
                            'r' => '\r',
                            '0' => '\0',
                            other => other,
                        });
                    }
                    other => text.push(other),
                }
            }
            tokens.push(match c {
                '"' => Token::Text(text),
                _ => Token::Char(text.chars().next().ok_or("an empty character")?),
            });
        } else {
            let rest: String = chars[i..chars.len().min(i + 2)].iter().collect();
            let op = OPERATORS
                .iter()
                .find(|op| rest.starts_with(**op))
                .ok_or_else(|| format!("{c} is not understood here"))?;
            tokens.push(Token::Op(op));
            i += op.chars().count();
        }
    }
    Ok(tokens)
}

/// Parse an expression ([`Expr`]); an error says what is not understood.
fn parse_expression(text: &str) -> Result<Expr, String> {
    let tokens = tokenize(text)?;
    if tokens.is_empty() {
        return Err("nothing to evaluate".into());
    }
    let mut parser = Parser { tokens, at: 0 };
    let expr = parser.binary(0)?;
    match parser.tokens.get(parser.at) {
        None => Ok(expr),
        Some(token) => Err(format!("{token:?} is not understood here")),
    }
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
}

/// Binary operators by precedence, loosest first.
const LEVELS: &[&[&str]] = &[
    &["||"],
    &["&&"],
    &["==", "!="],
    &["<", "<=", ">", ">="],
    &["+", "-"],
    &["*", "/", "%"],
];

impl Parser {
    fn peek_op(&self) -> Option<&'static str> {
        match self.tokens.get(self.at) {
            Some(Token::Op(op)) => Some(op),
            _ => None,
        }
    }

    fn expect(&mut self, op: &str) -> Result<(), String> {
        match self.peek_op() {
            Some(found) if found == op => {
                self.at += 1;
                Ok(())
            }
            _ => Err(format!("{op} expected")),
        }
    }

    fn binary(&mut self, level: usize) -> Result<Expr, String> {
        let Some(ops) = LEVELS.get(level) else {
            return self.unary();
        };
        let mut left = self.binary(level + 1)?;
        while let Some(op) = self.peek_op().filter(|op| ops.contains(op)) {
            self.at += 1;
            let right = self.binary(level + 1)?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr, String> {
        match self.peek_op() {
            Some("-") => {
                self.at += 1;
                Ok(Expr::Unary('-', Box::new(self.unary()?)))
            }
            Some("!") => {
                self.at += 1;
                Ok(Expr::Unary('!', Box::new(self.unary()?)))
            }
            _ => self.postfix(),
        }
    }

    fn arguments(&mut self) -> Result<Vec<Expr>, String> {
        self.expect("(")?;
        let mut args = Vec::new();
        if self.peek_op() == Some(")") {
            self.at += 1;
            return Ok(args);
        }
        loop {
            args.push(self.binary(0)?);
            match self.peek_op() {
                Some(",") => self.at += 1,
                Some(")") => {
                    self.at += 1;
                    return Ok(args);
                }
                _ => return Err(") expected".into()),
            }
        }
    }

    fn postfix(&mut self) -> Result<Expr, String> {
        let mut expr = self.primary()?;
        loop {
            match self.peek_op() {
                Some(".") => {
                    self.at += 1;
                    let Some(Token::Name(name)) = self.tokens.get(self.at).cloned() else {
                        return Err("a name expected after .".into());
                    };
                    self.at += 1;
                    expr = match self.peek_op() {
                        Some("(") => Expr::Call(Some(Box::new(expr)), name, self.arguments()?),
                        _ => Expr::Field(Box::new(expr), name),
                    };
                }
                Some("[") => {
                    self.at += 1;
                    let index = self.binary(0)?;
                    self.expect("]")?;
                    expr = Expr::Index(Box::new(expr), Box::new(index));
                }
                _ => return Ok(expr),
            }
        }
    }

    fn primary(&mut self) -> Result<Expr, String> {
        let token = self
            .tokens
            .get(self.at)
            .cloned()
            .ok_or("an expression ended early")?;
        self.at += 1;
        Ok(match token {
            Token::Int(n) => Expr::Int(n),
            Token::Long(n) => Expr::Long(n),
            Token::Double(n) => Expr::Double(n),
            Token::Text(t) => Expr::Text(t),
            Token::Char(c) => Expr::Char(c),
            Token::Name(name) => match name.as_str() {
                "true" => Expr::Bool(true),
                "false" => Expr::Bool(false),
                "null" => Expr::Null,
                "this" => Expr::This,
                _ if self.peek_op() == Some("(") => Expr::Call(None, name, self.arguments()?),
                _ => Expr::Name(name),
            },
            Token::Op("(") => {
                let inner = self.binary(0)?;
                self.expect(")")?;
                inner
            }
            Token::Op(op) => return Err(format!("{op} is not understood here")),
        })
    }
}

/// A value while an expression is worked out: one in the VM, or text not
/// put there (a literal, a concatenation) unless a call needs it.
#[derive(Debug, Clone)]
enum Val {
    J(JValue),
    Text(String),
}

/// A number while an expression is worked out.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Num {
    Int(i64),
    Long(i64),
    Double(f64),
}

/// The frame an expression is evaluated in.
struct Frame<'a> {
    vm: &'a Jdwp,
    thread: u64,
    depth: usize,
    location: Location,
    /// Whether methods may be called (not for a hover).
    calls: bool,
}

/// A frame's current id: frames are looked up again after anything that
/// ran the thread (an invoked method).
async fn frame_id(vm: &Jdwp, thread: u64, depth: usize) -> Result<u64, String> {
    vm.frames(thread)
        .await?
        .get(depth)
        .map(|(id, _)| *id)
        .ok_or_else(|| "no such frame".to_string())
}

type Eval<'b> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Val, String>> + Send + 'b>>;

impl Frame<'_> {
    fn eval<'b>(&'b self, expr: &'b Expr) -> Eval<'b> {
        Box::pin(async move {
            let vm = self.vm;
            Ok(match expr {
                Expr::Int(n) => Val::J(JValue {
                    tag: b'I',
                    bits: *n as i32 as u32 as u64,
                }),
                Expr::Long(n) => Val::J(JValue {
                    tag: b'J',
                    bits: *n as u64,
                }),
                Expr::Double(n) => Val::J(JValue {
                    tag: b'D',
                    bits: n.to_bits(),
                }),
                Expr::Bool(b) => Val::J(JValue {
                    tag: b'Z',
                    bits: u64::from(*b),
                }),
                Expr::Char(c) => Val::J(JValue {
                    tag: b'C',
                    bits: u64::from(*c as u32 as u16),
                }),
                Expr::Text(t) => Val::Text(t.clone()),
                Expr::Null => Val::J(JValue { tag: b'L', bits: 0 }),
                Expr::This => Val::J(self.this().await?.ok_or("no this in a static method")?),
                Expr::Name(name) => Val::J(self.name(name).await?),
                Expr::Field(target, name) => {
                    if let Expr::Name(class) = target.as_ref()
                        && !self.is_variable(class).await
                        && let Some(class) = self.class_named(class).await
                    {
                        return Ok(Val::J(static_field(vm, class, name).await?));
                    }
                    let target = self.in_vm(self.eval(target).await?).await?;
                    Val::J(field_of(vm, target, name).await?)
                }
                Expr::Index(target, index) => {
                    let target = self.in_vm(self.eval(target).await?).await?;
                    let index = self.unboxed(self.eval(index).await?).await;
                    let index = match number(&index) {
                        Some(Num::Int(i) | Num::Long(i)) if i >= 0 => i as usize,
                        _ => return Err("an index must be a whole number".into()),
                    };
                    Val::J(element_of(vm, target, index).await?)
                }
                Expr::Call(receiver, name, args) => {
                    Val::J(self.call(receiver.as_deref(), name, args).await?)
                }
                Expr::Unary('-', inner) => {
                    match number(&self.unboxed(self.eval(inner).await?).await) {
                        Some(Num::Int(i)) => num(Num::Int(-i)),
                        Some(Num::Long(i)) => num(Num::Long(-i)),
                        Some(Num::Double(d)) => num(Num::Double(-d)),
                        None => return Err("- needs a number".into()),
                    }
                }
                Expr::Unary(_, inner) => match self.unboxed(self.eval(inner).await?).await {
                    Val::J(JValue { tag: b'Z', bits }) => Val::J(JValue {
                        tag: b'Z',
                        bits: u64::from(bits == 0),
                    }),
                    _ => return Err("! needs a boolean".into()),
                },
                Expr::Binary(op, left, right) => self.binary(op, left, right).await?,
            })
        })
    }

    async fn binary(&self, op: &str, left: &Expr, right: &Expr) -> Result<Val, String> {
        let l = self.eval(left).await?;
        // `&&` and `||` look no further than they need to.
        if matches!(op, "&&" | "||") {
            let needs = || format!("{op} needs booleans");
            let l = boolean(&self.unboxed(l).await).ok_or_else(needs)?;
            if (op == "&&" && !l) || (op == "||" && l) {
                return Ok(bool_val(l));
            }
            let r = self.unboxed(self.eval(right).await?).await;
            return Ok(bool_val(boolean(&r).ok_or_else(needs)?));
        }
        let r = self.eval(right).await?;
        if op == "+" && (self.is_text(&l) || self.is_text(&r)) {
            return Ok(Val::Text(self.text(&l).await? + &self.text(&r).await?));
        }
        // Two objects are compared as references, as Java compares them;
        // otherwise a boxed number or boolean takes part as its value.
        let objects = |v: &Val| matches!(v, Val::J(j) if !is_primitive(j.tag));
        let (l, r) = match matches!(op, "==" | "!=") && objects(&l) && objects(&r) {
            true => (l, r),
            false => (self.unboxed(l).await, self.unboxed(r).await),
        };
        if let (Some(a), Some(b)) = (number(&l), number(&r)) {
            return arithmetic(op, a, b);
        }
        match (op, &l, &r) {
            ("==" | "!=", Val::J(a), Val::J(b)) => {
                let same = a.bits == b.bits
                    && (a.tag == b.tag || !is_primitive(a.tag) && !is_primitive(b.tag));
                Ok(bool_val(same == (op == "==")))
            }
            _ => Err(format!("{op} is not defined on these values")),
        }
    }

    /// A boxed value (`Integer`, `Boolean`, …) as its primitive; anything
    /// else as it is.
    async fn unboxed(&self, value: Val) -> Val {
        let Val::J(j) = &value else {
            return value;
        };
        if j.tag != b'L' || j.bits == 0 {
            return value;
        }
        let Ok(class) = self.vm.object_class(j.bits).await else {
            return value;
        };
        let boxes = [
            "Ljava/lang/Integer;",
            "Ljava/lang/Long;",
            "Ljava/lang/Double;",
            "Ljava/lang/Float;",
            "Ljava/lang/Short;",
            "Ljava/lang/Byte;",
            "Ljava/lang/Character;",
            "Ljava/lang/Boolean;",
        ];
        match self.vm.signature(class).await {
            Ok(signature) if boxes.contains(&signature.as_str()) => {
                match field_of(self.vm, *j, "value").await {
                    Ok(inner) => Val::J(inner),
                    Err(_) => value,
                }
            }
            _ => value,
        }
    }

    /// Run a method in the frame's thread — not for a hover.
    async fn invoke(
        &self,
        object: Option<u64>,
        class: u64,
        method: u64,
        args: &[JValue],
    ) -> Result<JValue, String> {
        if !self.calls {
            return Err("methods are not called for a hover".into());
        }
        self.vm
            .invoke(object, class, self.thread, method, args)
            .await
    }

    async fn this(&self) -> Result<Option<JValue>, String> {
        let frame = frame_id(self.vm, self.thread, self.depth).await?;
        let this = self.vm.this_object(self.thread, frame).await?;
        Ok((this.bits != 0).then_some(this))
    }

    async fn is_variable(&self, name: &str) -> bool {
        self.name(name).await.is_ok()
    }

    /// A local, a field of `this`, or a static field of the frame's class.
    async fn name(&self, name: &str) -> Result<JValue, String> {
        let frame = frame_id(self.vm, self.thread, self.depth).await?;
        let scope = locals(self.vm, self.thread, frame, self.location).await?;
        if let Some((_, value)) = scope.iter().find(|(n, _)| n == name) {
            return Ok(*value);
        }
        if let Some((_, this)) = scope.iter().find(|(n, _)| n == "this")
            && let Ok(value) = field_of(self.vm, *this, name).await
        {
            return Ok(value);
        }
        static_field(self.vm, self.location.class, name)
            .await
            .map_err(|_| format!("no variable {name} here"))
    }

    /// A class by the name the code would use: in the frame's package, or
    /// `java.lang`, or fully named.
    async fn class_named(&self, name: &str) -> Option<u64> {
        let own = class_name(&self.vm.signature(self.location.class).await.ok()?);
        let package = own.rsplit_once('.').map(|(p, _)| p.to_string());
        let mut candidates = vec![name.to_string(), format!("java.lang.{name}")];
        if let Some(package) = package {
            candidates.insert(0, format!("{package}.{name}"));
        }
        for candidate in candidates {
            let signature = format!("L{};", candidate.replace('.', "/"));
            if let Some(class) = self.vm.classes_by_signature(&signature).await.ok()?.first() {
                return Some(*class);
            }
        }
        None
    }

    /// Call `name` on `receiver` — an object, a class by name, or (none)
    /// `this` or the frame's class.
    async fn call(
        &self,
        receiver: Option<&Expr>,
        name: &str,
        args: &[Expr],
    ) -> Result<JValue, String> {
        let (object, class) = match receiver {
            Some(Expr::Name(class_name)) if !self.is_variable(class_name).await => {
                let class = self
                    .class_named(class_name)
                    .await
                    .ok_or_else(|| format!("no variable or class {class_name} here"))?;
                (None, class)
            }
            Some(receiver) => {
                let object = self.in_vm(self.eval(receiver).await?).await?;
                if object.bits == 0 {
                    return Err("it is null".into());
                }
                (Some(object.bits), self.vm.object_class(object.bits).await?)
            }
            None => match self.this().await? {
                Some(this) => (Some(this.bits), self.vm.object_class(this.bits).await?),
                None => (None, self.location.class),
            },
        };
        let mut values = Vec::new();
        for arg in args {
            values.push(self.eval(arg).await?);
        }
        let candidates = overloads(self.vm, class, name, values.len()).await?;
        if candidates.is_empty() {
            return Err(format!(
                "no method {name} taking {} arguments",
                values.len()
            ));
        }
        let mut kinds = Vec::new();
        for value in &values {
            kinds.push(self.kind(value).await?);
        }
        let method = candidates
            .into_iter()
            .filter_map(|m| {
                let params = parameter_types(&m.signature);
                let scores = kinds.iter().zip(&params).map(|(k, p)| k.fits(p));
                scores.sum::<Option<u32>>().map(|score| (score, m))
            })
            .min_by_key(|(score, _)| *score)
            .map(|(_, m)| m)
            .ok_or_else(|| format!("no method {name} takes these arguments"))?;
        let mut converted = Vec::new();
        for (value, param) in values.into_iter().zip(parameter_types(&method.signature)) {
            converted.push(self.argument(value, &param).await?);
        }
        let object = match method.is_static {
            true => None,
            false => Some(object.ok_or_else(|| format!("{name} needs an object to call it on"))?),
        };
        self.invoke(object, method.declaring, method.id, &converted)
            .await
    }

    /// What a value is, for choosing among a method's overloads.
    async fn kind(&self, value: &Val) -> Result<Kind, String> {
        let class = match value {
            Val::J(j) if is_primitive(j.tag) => return Ok(Kind::Primitive(j.tag)),
            Val::J(j) if j.bits == 0 => return Ok(Kind::Null),
            Val::J(j) => self.vm.object_class(j.bits).await?,
            Val::Text(_) => *self
                .vm
                .classes_by_signature("Ljava/lang/String;")
                .await?
                .first()
                .ok_or("no String class")?,
        };
        let types = supertypes(self.vm, class).await?;
        Ok(Kind::Reference(types.into_iter().map(|(_, s)| s).collect()))
    }

    /// A value as a parameter of type `param` takes it: widened, boxed or
    /// unboxed, text put in the VM as a string.
    async fn argument(&self, value: Val, param: &str) -> Result<JValue, String> {
        let tag = param.as_bytes().first().copied().unwrap_or(b'L');
        if is_primitive(tag) {
            let value = self.unboxed(value).await;
            if tag == b'Z' {
                return boolean(&value)
                    .map(|b| JValue {
                        tag,
                        bits: u64::from(b),
                    })
                    .ok_or_else(|| "a boolean expected".into());
            }
            let n = number(&value).ok_or("a number expected")?;
            let whole = match n {
                Num::Int(i) | Num::Long(i) => i,
                Num::Double(d) => d as i64,
            };
            let bits = match tag {
                b'B' => u64::from(whole as i8 as u8),
                b'S' => u64::from(whole as i16 as u16),
                b'C' => u64::from(whole as u16),
                b'I' => u64::from(whole as i32 as u32),
                b'J' => whole as u64,
                b'F' => u64::from((to_f64(n) as f32).to_bits()),
                _ => to_f64(n).to_bits(),
            };
            return Ok(JValue { tag, bits });
        }
        match value {
            // Boxed, as Java boxes it: `Integer.valueOf(int)` and the like.
            Val::J(j) if is_primitive(j.tag) => {
                let boxed = box_of(j.tag);
                let class = *self
                    .vm
                    .classes_by_signature(boxed)
                    .await?
                    .first()
                    .ok_or_else(|| format!("{} is not loaded", class_name(boxed)))?;
                let signature = format!("({}){boxed}", j.tag as char);
                let method = self
                    .vm
                    .methods(class)
                    .await?
                    .into_iter()
                    .find(|(_, n, s, _)| n == "valueOf" && *s == signature)
                    .ok_or("no valueOf to box with")?;
                self.invoke(None, class, method.0, &[j]).await
            }
            other => self.in_vm(other).await,
        }
    }

    /// A value as the VM holds it — text put there as a string.
    async fn in_vm(&self, value: Val) -> Result<JValue, String> {
        match value {
            Val::J(j) => Ok(j),
            Val::Text(text) => self.vm.create_string(&text).await,
        }
    }

    fn is_text(&self, value: &Val) -> bool {
        matches!(value, Val::Text(_) | Val::J(JValue { tag: b's', .. }))
    }

    /// A value as `+` joins it to text: a string's text, a primitive as
    /// Java writes it, an object by its `toString()`.
    async fn text(&self, value: &Val) -> Result<String, String> {
        Ok(match value {
            Val::Text(text) => text.clone(),
            Val::J(j) if j.tag == b's' && j.bits != 0 => self.vm.string_value(j.bits).await?,
            Val::J(j) if j.bits == 0 && !is_primitive(j.tag) => "null".into(),
            Val::J(j) if is_primitive(j.tag) => {
                let shown = display(self.vm, *j).await;
                shown.trim_matches('\'').to_string()
            }
            Val::J(j) => {
                let class = self.vm.object_class(j.bits).await?;
                match overloads(self.vm, class, "toString", 0).await?.first() {
                    Some(method) => {
                        let text = self
                            .invoke(Some(j.bits), method.declaring, method.id, &[])
                            .await?;
                        match text.bits {
                            0 => "null".into(),
                            bits => self.vm.string_value(bits).await?,
                        }
                    }
                    None => display(self.vm, *j).await,
                }
            }
        })
    }
}

fn is_primitive(tag: u8) -> bool {
    matches!(tag, b'B' | b'C' | b'F' | b'D' | b'I' | b'J' | b'S' | b'Z')
}

fn bool_val(b: bool) -> Val {
    Val::J(JValue {
        tag: b'Z',
        bits: u64::from(b),
    })
}

fn boolean(value: &Val) -> Option<bool> {
    match value {
        Val::J(JValue { tag: b'Z', bits }) => Some(*bits != 0),
        _ => None,
    }
}

fn number(value: &Val) -> Option<Num> {
    let Val::J(j) = value else {
        return None;
    };
    Some(match j.tag {
        b'I' => Num::Int(j.bits as u32 as i32 as i64),
        b'S' => Num::Int(j.bits as u16 as i16 as i64),
        b'B' => Num::Int(j.bits as u8 as i8 as i64),
        b'C' => Num::Int(j.bits as u16 as i64),
        b'J' => Num::Long(j.bits as i64),
        b'F' => Num::Double(f32::from_bits(j.bits as u32) as f64),
        b'D' => Num::Double(f64::from_bits(j.bits)),
        _ => return None,
    })
}

fn to_f64(n: Num) -> f64 {
    match n {
        Num::Int(i) | Num::Long(i) => i as f64,
        Num::Double(d) => d,
    }
}

fn num(n: Num) -> Val {
    Val::J(match n {
        Num::Int(i) => JValue {
            tag: b'I',
            bits: i as i32 as u32 as u64,
        },
        Num::Long(i) => JValue {
            tag: b'J',
            bits: i as u64,
        },
        Num::Double(d) => JValue {
            tag: b'D',
            bits: d.to_bits(),
        },
    })
}

/// Java's arithmetic and comparisons: `int` unless a `long` or a floating
/// value takes part, integer division by zero refused as Java refuses it.
fn arithmetic(op: &str, a: Num, b: Num) -> Result<Val, String> {
    let floating = matches!(a, Num::Double(_)) || matches!(b, Num::Double(_));
    let long = matches!(a, Num::Long(_)) || matches!(b, Num::Long(_));
    if floating {
        let (x, y) = (to_f64(a), to_f64(b));
        return Ok(match op {
            "+" => num(Num::Double(x + y)),
            "-" => num(Num::Double(x - y)),
            "*" => num(Num::Double(x * y)),
            "/" => num(Num::Double(x / y)),
            "%" => num(Num::Double(x % y)),
            "<" => bool_val(x < y),
            "<=" => bool_val(x <= y),
            ">" => bool_val(x > y),
            ">=" => bool_val(x >= y),
            "==" => bool_val(x == y),
            "!=" => bool_val(x != y),
            _ => return Err(format!("{op} is not defined on numbers")),
        });
    }
    let (Num::Int(x) | Num::Long(x), Num::Int(y) | Num::Long(y)) = (a, b) else {
        unreachable!("integral");
    };
    let wrap = |v: i64| match long {
        true => num(Num::Long(v)),
        false => num(Num::Int(v as i32 as i64)),
    };
    Ok(match op {
        "+" => wrap(x.wrapping_add(y)),
        "-" => wrap(x.wrapping_sub(y)),
        "*" => wrap(x.wrapping_mul(y)),
        "/" | "%" if y == 0 => {
            return Err("it threw java.lang.ArithmeticException: / by zero".into());
        }
        "/" => wrap(x.wrapping_div(y)),
        "%" => wrap(x.wrapping_rem(y)),
        "<" => bool_val(x < y),
        "<=" => bool_val(x <= y),
        ">" => bool_val(x > y),
        ">=" => bool_val(x >= y),
        "==" => bool_val(x == y),
        "!=" => bool_val(x != y),
        _ => return Err(format!("{op} is not defined on numbers")),
    })
}

/// A static field of `class` (or its superclasses).
async fn static_field(vm: &Jdwp, class: u64, name: &str) -> Result<JValue, String> {
    let mut current = class;
    for _ in 0..16 {
        if current == 0 {
            break;
        }
        if let Some(field) = vm
            .fields(current)
            .await?
            .into_iter()
            .find(|f| f.1 == name && f.3 & 0x0008 != 0)
        {
            return vm
                .static_values(current, &[field.0])
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| format!("no field {name}"));
        }
        current = vm.superclass(current).await.unwrap_or(0);
    }
    Err(format!("no field {name}"))
}

/// A method that can be called.
struct Method {
    /// The type that declares it.
    declaring: u64,
    id: u64,
    signature: String,
    is_static: bool,
}

/// A type and the types it is also: its superclasses, then the interfaces
/// of each — by id and signature, the type itself first.
async fn supertypes(vm: &Jdwp, class: u64) -> Result<Vec<(u64, String)>, String> {
    let mut chain = Vec::new();
    let mut current = class;
    while current != 0 && chain.len() < 32 {
        chain.push(current);
        current = vm.superclass(current).await.unwrap_or(0);
    }
    let mut queue: std::collections::VecDeque<u64> = chain.iter().copied().collect();
    let mut seen = std::collections::HashSet::new();
    let mut types = Vec::new();
    while let Some(next) = queue.pop_front() {
        if !seen.insert(next) || types.len() >= 128 {
            continue;
        }
        types.push((next, vm.signature(next).await?));
        queue.extend(vm.interfaces(next).await.unwrap_or_default());
    }
    Ok(types)
}

/// The methods `name` taking `arity` arguments that `class` has: its own,
/// then those it inherits and does not override.
async fn overloads(vm: &Jdwp, class: u64, name: &str, arity: usize) -> Result<Vec<Method>, String> {
    let mut found: Vec<Method> = Vec::new();
    for (declaring, _) in supertypes(vm, class).await? {
        for (id, n, signature, modifiers) in vm.methods(declaring).await? {
            if n != name
                || parameter_types(&signature).len() != arity
                || found.iter().any(|m| m.signature == signature)
            {
                continue;
            }
            found.push(Method {
                declaring,
                id,
                signature,
                is_static: modifiers & 0x0008 != 0,
            });
        }
    }
    Ok(found)
}

/// What an argument is, for choosing among overloads: a primitive by its
/// tag, `null`, or an object by every type it is (its own first).
#[derive(Debug, Clone, PartialEq)]
enum Kind {
    Primitive(u8),
    Null,
    Reference(Vec<String>),
}

/// The class a primitive is boxed in.
fn box_of(tag: u8) -> &'static str {
    match tag {
        b'Z' => "Ljava/lang/Boolean;",
        b'B' => "Ljava/lang/Byte;",
        b'C' => "Ljava/lang/Character;",
        b'S' => "Ljava/lang/Short;",
        b'I' => "Ljava/lang/Integer;",
        b'J' => "Ljava/lang/Long;",
        b'F' => "Ljava/lang/Float;",
        _ => "Ljava/lang/Double;",
    }
}

/// Whether Java widens primitive `from` to `to` without a cast.
fn widens(from: u8, to: u8) -> bool {
    let targets: &[u8] = match from {
        b'B' => b"SIJFD",
        b'S' | b'C' => b"IJFD",
        b'I' => b"JFD",
        b'J' => b"FD",
        b'F' => b"D",
        _ => b"",
    };
    targets.contains(&to)
}

impl Kind {
    /// How well this fits a parameter of type `param` — 0 exactly, more the
    /// further Java must go (widen, box, generalise) — or not at all.
    fn fits(&self, param: &str) -> Option<u32> {
        let tag = param.as_bytes().first().copied()?;
        match self {
            Kind::Primitive(from) if is_primitive(tag) => match *from == tag {
                true => Some(0),
                false => widens(*from, tag).then_some(1),
            },
            Kind::Primitive(from) => {
                let general = [
                    "Ljava/lang/Object;",
                    "Ljava/io/Serializable;",
                    "Ljava/lang/Comparable;",
                ];
                match param {
                    _ if param == box_of(*from) => Some(2),
                    "Ljava/lang/Number;" if *from != b'Z' && *from != b'C' => Some(3),
                    _ if general.contains(&param) => Some(4),
                    _ => None,
                }
            }
            Kind::Null => (!is_primitive(tag)).then_some(1),
            Kind::Reference(types) if is_primitive(tag) => {
                let own = types.first()?;
                let from = *b"ZBCSIJFD".iter().find(|t| box_of(**t) == own)?;
                match from == tag {
                    true => Some(2),
                    false => widens(from, tag).then_some(3),
                }
            }
            Kind::Reference(types) => match types.iter().position(|t| t == param) {
                Some(0) => Some(0),
                Some(_) if param == "Ljava/lang/Object;" => Some(4),
                Some(_) => Some(1),
                None => None,
            },
        }
    }
}

/// A method signature's parameter types (`(I[JLjava/lang/String;)V` →
/// `I`, `[J`, `Ljava/lang/String;`).
fn parameter_types(signature: &str) -> Vec<String> {
    let inner = signature
        .strip_prefix('(')
        .and_then(|s| s.split_once(')'))
        .map(|(params, _)| params)
        .unwrap_or("");
    let mut types = Vec::new();
    let bytes = inner.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        while bytes[i] == b'[' {
            i += 1;
        }
        if bytes[i] == b'L' {
            while i < bytes.len() && bytes[i] != b';' {
                i += 1;
            }
        }
        i += 1;
        types.push(inner[start..i.min(inner.len())].to_string());
    }
    types
}

/// The package a Java source declares.
fn package_of(source: &str) -> Option<String> {
    for line in source.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("package ") {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '$'))
                .collect();
            return (!name.is_empty()).then_some(name);
        }
        if line.starts_with("import ") || line.starts_with("public ") || line.starts_with("class ")
        {
            return None;
        }
    }
    None
}

/// The directory a source file's package path starts at.
fn source_root(program: &Path, package: Option<&str>) -> PathBuf {
    let mut root = program.parent().unwrap_or(Path::new(".")).to_path_buf();
    if let Some(package) = package {
        for _ in package.split('.') {
            root = root.parent().map(Path::to_path_buf).unwrap_or(root);
        }
    }
    root
}

/// Where a Maven, Gradle or plain project keeps its sources.
fn source_roots(root: &Path) -> Vec<PathBuf> {
    [
        "src/main/java",
        "src/test/java",
        "src/main/kotlin",
        "src",
        ".",
    ]
    .iter()
    .map(|r| root.join(r))
    .filter(|r| r.is_dir())
    .collect()
}

/// A build's class output: Maven's `target/classes`, Gradle's
/// `build/classes/java/main`, theirs for tests, an IDE's `bin/` and `out/`.
fn build_outputs(root: &Path) -> Vec<PathBuf> {
    [
        "target/classes",
        "target/test-classes",
        "build/classes/java/main",
        "build/classes/java/test",
        "bin",
    ]
    .iter()
    .map(|r| root.join(r))
    .filter(|r| r.is_dir())
    .collect()
}

/// The port in `Listening for transport dt_socket at address: 34567`, the
/// agent's line; the JVM's own notes before it are passed over.
async fn listening_port<R>(stdout: &mut BufReader<R>) -> Result<u16, String>
where
    R: AsyncRead + Unpin,
{
    for _ in 0..50 {
        let mut line = String::new();
        let n = stdout
            .read_line(&mut line)
            .await
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("the JVM ended before its debugger listened".into());
        }
        if let Some(at) = line.find("at address:") {
            let address = line[at + "at address:".len()..].trim();
            let port = address.rsplit(':').next().unwrap_or(address);
            return port
                .parse()
                .map_err(|_| format!("the JVM's debugger listens at {address:?}"));
        }
    }
    Err("the JVM did not say where its debugger listens".into())
}

/// A directory of the session's own: made new, never one that was there.
fn scratch_dir() -> Result<PathBuf, String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut last = String::new();
    for _ in 0..16 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "clew-java-scratch-{}-{}-{nanos}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // (Only Unix has a mode to set; on Windows the directory takes its
        // parent's per-user access list.)
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        match builder.create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = e.to_string(),
            Err(e) => return Err(format!("cannot make {}: {e}", dir.display())),
        }
    }
    Err(format!("cannot make a scratch directory: {last}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dapout::testing::Client;

    #[test]
    fn signatures_paths_and_lines_are_read() {
        assert_eq!(class_name("Ldemo/Main$Cart;"), "demo.Main$Cart");
        assert_eq!(type_name("[I"), "int[]");
        assert_eq!(type_name("Ljava/lang/String;"), "String");
        assert_eq!(line_at(&[(0, 10), (4, 11), (9, 12)], 6), Some(11));
        assert_eq!(line_at(&[(3, 10)], 1), None);
        assert_eq!(package_of("import java.util.List;\nclass A {}"), None);
        assert_eq!(
            source_root(Path::new("/p/src/demo/shop/Main.java"), Some("demo.shop")),
            Path::new("/p/src")
        );
        let sizes = IdSizes {
            field: 4,
            method: 4,
            object: 8,
            reference: 8,
            frame: 8,
        };
        let written = Writer::new(sizes)
            .u8(7)
            .method(0x0102)
            .object(5)
            .string("ab")
            .bytes;
        assert_eq!(
            written,
            [
                7, 0, 0, 1, 2, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 2, b'a', b'b'
            ]
        );
        let mut r = Reader::new(&written, sizes);
        assert_eq!(
            (r.u8(), r.method(), r.object(), r.string()),
            (Ok(7), Ok(0x0102), Ok(5), Ok("ab".into()))
        );
        assert!(r.u8().is_err(), "past the end is an error, not a panic");
    }

    #[test]
    fn expressions_are_parsed_with_java_precedence() {
        let e = |t: &str| parse_expression(t).unwrap();
        let name = |n: &str| Box::new(Expr::Name(n.into()));
        assert_eq!(
            e("a + b * 2"),
            Expr::Binary(
                "+",
                name("a"),
                Box::new(Expr::Binary("*", name("b"), Box::new(Expr::Int(2))))
            )
        );
        assert_eq!(
            e("a.b[1].c(2, \"x\")"),
            Expr::Call(
                Some(Box::new(Expr::Index(
                    Box::new(Expr::Field(name("a"), "b".into())),
                    Box::new(Expr::Int(1))
                ))),
                "c".into(),
                vec![Expr::Int(2), Expr::Text("x".into())]
            )
        );
        assert_eq!(
            e("!a && b || c"),
            Expr::Binary(
                "||",
                Box::new(Expr::Binary(
                    "&&",
                    Box::new(Expr::Unary('!', name("a"))),
                    name("b")
                )),
                name("c")
            )
        );
        assert_eq!(
            e("-(1 - 2) <= 3L"),
            Expr::Binary(
                "<=",
                Box::new(Expr::Unary(
                    '-',
                    Box::new(Expr::Binary(
                        "-",
                        Box::new(Expr::Int(1)),
                        Box::new(Expr::Int(2))
                    ))
                )),
                Box::new(Expr::Long(3))
            )
        );
        assert_eq!(e("f()"), Expr::Call(None, "f".into(), vec![]));
        assert_eq!(e("this.x"), Expr::Field(Box::new(Expr::This), "x".into()));
        assert_eq!(e("1_000"), Expr::Int(1000));
        assert_eq!(e("2.5e1"), Expr::Double(25.0));
        assert_eq!(e("1f"), Expr::Double(1.0));
        assert_eq!(e("'\\n'"), Expr::Char('\n'));
        assert_eq!(e("\"a\\\"b\""), Expr::Text("a\"b".into()));
        assert_eq!(
            e("true != null"),
            Expr::Binary("!=", Box::new(Expr::Bool(true)), Box::new(Expr::Null))
        );
        for bad in [
            "", "a +", "a.", "f(1,", "(a", "\"open", "a # b", "a b", "a[1",
        ] {
            assert!(parse_expression(bad).is_err(), "{bad:?} is refused");
        }
    }

    #[test]
    fn arithmetic_is_javas() {
        let shown = |v: Result<Val, String>| match v {
            Ok(Val::J(j)) => (j.tag, j.bits),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            shown(arithmetic("+", Num::Int(i32::MAX as i64), Num::Int(1))),
            (b'I', i32::MIN as u32 as u64),
            "int wraps"
        );
        assert_eq!(
            shown(arithmetic("+", Num::Long(i32::MAX as i64), Num::Int(1))),
            (b'J', 1u64 << 31),
            "a long takes part: long"
        );
        assert_eq!(
            shown(arithmetic("/", Num::Int(-7), Num::Int(2))),
            (b'I', (-3i32) as u32 as u64)
        );
        assert_eq!(
            shown(arithmetic("%", Num::Int(-7), Num::Int(2))),
            (b'I', (-1i32) as u32 as u64)
        );
        assert_eq!(
            shown(arithmetic("/", Num::Int(1), Num::Double(2.0))),
            (b'D', 0.5f64.to_bits())
        );
        assert_eq!(
            shown(arithmetic("<", Num::Int(1), Num::Double(1.5))),
            (b'Z', 1)
        );
        assert_eq!(
            shown(arithmetic("==", Num::Long(3), Num::Int(3))),
            (b'Z', 1)
        );
        assert!(
            arithmetic("/", Num::Int(1), Num::Int(0))
                .unwrap_err()
                .contains("by zero")
        );
        assert!(arithmetic("%", Num::Long(1), Num::Long(0)).is_err());
        assert_eq!(
            shown(arithmetic("/", Num::Double(1.0), Num::Int(0))),
            (b'D', f64::INFINITY.to_bits())
        );
        assert!(arithmetic("&&", Num::Int(1), Num::Int(1)).is_err());
        assert_eq!(java_float(3.0), "3.0");
        assert_eq!(java_float(f64::NAN), "NaN");
        assert_eq!(java_float(f64::NEG_INFINITY), "-Infinity");
    }

    #[test]
    fn overloads_are_chosen_as_java_chooses_them() {
        let int = Kind::Primitive(b'I');
        assert_eq!(int.fits("I"), Some(0));
        assert_eq!(int.fits("J"), Some(1));
        assert_eq!(int.fits("D"), Some(1));
        assert_eq!(int.fits("C"), None, "int does not narrow to char");
        assert_eq!(int.fits("Z"), None);
        assert_eq!(int.fits("Ljava/lang/Integer;"), Some(2));
        assert_eq!(int.fits("Ljava/lang/Number;"), Some(3));
        assert_eq!(int.fits("Ljava/lang/Object;"), Some(4));
        assert_eq!(
            int.fits("Ljava/lang/Long;"),
            None,
            "int does not box to Long"
        );
        assert_eq!(int.fits("[C"), None);
        assert_eq!(Kind::Primitive(b'Z').fits("Ljava/lang/Number;"), None);
        assert_eq!(Kind::Null.fits("Ljava/lang/String;"), Some(1));
        assert_eq!(Kind::Null.fits("I"), None);
        let string = Kind::Reference(vec![
            "Ljava/lang/String;".into(),
            "Ljava/lang/Object;".into(),
            "Ljava/lang/CharSequence;".into(),
        ]);
        assert_eq!(string.fits("Ljava/lang/String;"), Some(0));
        assert_eq!(string.fits("Ljava/lang/CharSequence;"), Some(1));
        assert_eq!(string.fits("Ljava/lang/Object;"), Some(4));
        assert_eq!(string.fits("Ljava/lang/Integer;"), None);
        assert_eq!(string.fits("I"), None);
        let boxed = Kind::Reference(vec![
            "Ljava/lang/Integer;".into(),
            "Ljava/lang/Object;".into(),
        ]);
        assert_eq!(boxed.fits("I"), Some(2), "unboxed");
        assert_eq!(boxed.fits("J"), Some(3), "unboxed and widened");
        assert_eq!(boxed.fits("S"), None);
        assert_eq!(
            parameter_types("(I[JLjava/lang/String;[[Ljava/lang/Object;Z)V"),
            ["I", "[J", "Ljava/lang/String;", "[[Ljava/lang/Object;", "Z"]
        );
        assert!(parameter_types("()V").is_empty());
        for (from, to, ok) in [
            (b'B', b'S', true),
            (b'C', b'S', false),
            (b'I', b'F', true),
            (b'D', b'F', false),
            (b'Z', b'I', false),
        ] {
            assert_eq!(widens(from, to), ok, "{} to {}", from as char, to as char);
        }
    }

    /// A running Java program paused: its threads listed and stopped, its
    /// variables read, but no method called — the VM runs one only in a
    /// thread an event stopped — and the program run on to its end.
    #[tokio::test]
    #[ignore = "needs a JDK; run explicitly"]
    async fn a_running_java_program_is_paused() {
        let dir = clew_core::testutil::TempDir::new("java-pause-project");
        let program = dir.join("Spin.java");
        std::fs::write(
            &program,
            "public class Spin {\n    static volatile boolean go = true;\n\n    public static void main(String[] args) throws Exception {\n        String word = \"spin\";\n        long n = 0;\n        while (go && n < 400) {\n            n++;\n            Thread.sleep(10);\n        }\n        System.out.println(word + \" done\");\n    }\n}\n",
        )
        .unwrap();
        let tool = |name| clew_core::lsp::store::find_on_path_unresolved(name).expect(name);
        let (mut client, input, output) = Client::pipes();
        let running = tokio::spawn(serve(tool("java"), tool("javac"), input, output));
        client
            .request("initialize", json!({ "adapterID": "clew" }))
            .await;
        let launch = client
            .send(
                "launch",
                json!({ "program": program.to_string_lossy(), "cwd": dir.to_string_lossy() }),
            )
            .await;
        assert_eq!(client.answer(launch).await["success"], true);
        client.event("initialized").await;
        client.request("configurationDone", json!({})).await;
        tokio::time::sleep(Duration::from_millis(800)).await;
        let threads = client.request("threads", json!({})).await;
        let main = threads["threads"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "main")
            .cloned()
            .unwrap_or_else(|| panic!("main in {threads}"));
        client
            .request("pause", json!({ "threadId": main["id"] }))
            .await;
        assert_eq!(client.event("stopped").await["body"]["reason"], "pause");
        let stack = client
            .request("stackTrace", json!({ "threadId": main["id"] }))
            .await;
        let frame = stack["stackFrames"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == "Spin.main")
            .cloned()
            .unwrap_or_else(|| panic!("Spin.main in {stack}"));
        let read = client
            .request(
                "evaluate",
                json!({ "expression": "word + \"!\"", "frameId": frame["id"] }),
            )
            .await;
        assert_eq!(read["result"], "\"spin!\"");
        let seq = client
            .send(
                "evaluate",
                json!({ "expression": "word.length()", "frameId": frame["id"] }),
            )
            .await;
        let refused = client.answer(seq).await;
        assert_eq!(refused["success"], false, "{refused}");
        assert!(
            refused["message"]
                .as_str()
                .unwrap()
                .contains("stopped at a breakpoint or a step"),
            "{refused}"
        );
        client
            .request("continue", json!({ "threadId": main["id"] }))
            .await;
        assert_eq!(client.event("exited").await["body"]["exitCode"], 0);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !client.output().contains("spin done") && std::time::Instant::now() < deadline {
            client.gather(Duration::from_millis(500)).await;
        }
        assert!(client.output().contains("spin done"), "{}", client.output());
        client.request("disconnect", json!({})).await;
        assert_eq!(running.await.unwrap(), Ok(()));
    }

    /// A Java program debugged for real, over JDWP: compiled for the
    /// session, a breakpoint set when its class loads, the stack read with
    /// each frame at its line, locals, an object's fields, an array and a
    /// string opened, a path read, stepped, stopped again, and run to its
    /// end with its output. Needs a JDK (`java`, `javac`).
    #[tokio::test]
    #[ignore = "needs a JDK; run explicitly"]
    async fn a_java_program_is_debugged_over_jdwp() {
        let dir = clew_core::testutil::TempDir::new("java-dbg-project");
        std::fs::create_dir_all(dir.join("src/demo")).unwrap();
        let program = dir.join("src/demo/Main.java");
        std::fs::write(
            &program,
            "package demo;\n\npublic class Main {\n    static class Cart {\n        String owner = \"ann\";\n        int[] items = {2, 3};\n    }\n\n    static int total(Cart cart) {\n        int sum = 0;\n        for (int i : cart.items) {\n            sum += i;\n        }\n        return sum * 2;\n    }\n\n    public static void main(String[] args) {\n        Cart cart = new Cart();\n        int t = total(cart);\n        System.out.println(\"total \" + t + \" \" + args.length);\n    }\n\n    static java.util.List<Integer> seen = new java.util.ArrayList<>(java.util.List.of(4, 5));\n\n    static String greet(String who, int times) {\n        return \"hi \" + who.repeat(times);\n    }\n}\n",
        )
        .unwrap();
        let tool = |name| clew_core::lsp::store::find_on_path_unresolved(name).expect(name);
        let (mut client, input, output) = Client::pipes();
        let running = tokio::spawn(serve(tool("java"), tool("javac"), input, output));
        client
            .request("initialize", json!({ "adapterID": "clew" }))
            .await;
        let launch = client
            .send(
                "launch",
                json!({ "program": program.to_string_lossy(), "args": ["x"], "cwd": dir.to_string_lossy() }),
            )
            .await;
        let launched = client.answer(launch).await;
        assert_eq!(launched["success"], true, "{launched}: {:?}", client.seen);
        client.event("initialized").await;
        let source = json!({ "path": program.to_string_lossy() });
        let set = client
            .request(
                "setBreakpoints",
                json!({ "source": source, "breakpoints": [{ "line": 10 }, { "line": 20 }] }),
            )
            .await;
        assert_eq!(
            set["breakpoints"][0]["verified"], false,
            "its class is not loaded yet: {set}"
        );
        client.request("configurationDone", json!({})).await;

        let stopped = client.event("stopped").await;
        assert_eq!(stopped["body"]["reason"], "breakpoint");
        let changed = client.event("breakpoint").await;
        assert_eq!(changed["body"]["breakpoint"]["verified"], true);
        let thread = stopped["body"]["threadId"].clone();
        let threads = client.request("threads", json!({})).await;
        assert!(
            threads["threads"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["id"] == thread)
        );
        let stack = client
            .request("stackTrace", json!({ "threadId": thread }))
            .await;
        let frames = stack["stackFrames"].as_array().unwrap();
        assert_eq!(frames[0]["name"], "Main.total");
        assert_eq!(frames[0]["line"], 10);
        assert_eq!(
            frames[0]["source"]["path"],
            program.to_string_lossy().as_ref()
        );
        assert_eq!(frames[1]["name"], "Main.main");
        assert_eq!(frames[1]["line"], 19);
        let scopes = client
            .request("scopes", json!({ "frameId": frames[0]["id"] }))
            .await;
        let locals = client
            .request(
                "variables",
                json!({ "variablesReference": scopes["scopes"][0]["variablesReference"] }),
            )
            .await;
        let cart = locals["variables"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "cart")
            .cloned()
            .unwrap_or_else(|| panic!("cart in {locals}"));
        assert!(
            cart["value"].as_str().unwrap().starts_with("Main$Cart@"),
            "{cart}"
        );
        let fields = client
            .request(
                "variables",
                json!({ "variablesReference": cart["variablesReference"] }),
            )
            .await;
        let field = |name: &str| {
            fields["variables"]
                .as_array()
                .unwrap()
                .iter()
                .find(|v| v["name"] == name)
                .cloned()
                .unwrap_or_else(|| panic!("{name} in {fields}"))
        };
        assert_eq!(field("owner")["value"], "\"ann\"");
        assert_eq!(field("items")["value"], "int[2]");
        let items = client
            .request(
                "variables",
                json!({ "variablesReference": field("items")["variablesReference"] }),
            )
            .await;
        assert_eq!(items["variables"][1]["name"], "[1]");
        assert_eq!(items["variables"][1]["value"], "3");
        // Expressions, worked out over JDWP: fields, elements, arithmetic,
        // calls (instance, static, overloaded, boxing and unboxing), and a
        // breakpoint in a called method passed rather than stopped at.
        let frame = frames[0]["id"].clone();
        let cases = [
            ("cart.items[1]", "3"),
            ("cart.items[0] + cart.items[1]", "5"),
            ("cart.owner.length()", "3"),
            ("cart.owner.toUpperCase()", "\"ANN\""),
            ("greet(cart.owner + \"!\", 2)", "\"hi ann!ann!\""),
            ("total(cart)", "10"),
            ("seen.get(1) * 2", "10"),
            ("seen.size() == 2 && !cart.owner.isEmpty()", "true"),
            ("seen.contains(5)", "true"),
            ("Math.max(3, 7)", "7"),
            ("String.valueOf(42)", "\"42\""),
            ("Integer.MAX_VALUE + 1", "-2147483648"),
            ("1.5 * 2", "3.0"),
            ("\"n=\" + cart.items.length", "\"n=2\""),
            ("cart.owner == null", "false"),
            ("-(cart.items[0] - 7) % 4", "1"),
        ];
        for (expression, expected) in cases {
            let answer = client
                .request(
                    "evaluate",
                    json!({ "expression": expression, "frameId": frame, "context": "repl" }),
                )
                .await;
            assert_eq!(answer["result"], expected, "{expression}: {answer}");
        }
        let object = client
            .request(
                "evaluate",
                json!({ "expression": "cart", "frameId": frame }),
            )
            .await;
        assert!(
            object["variablesReference"].as_u64().unwrap() > 0,
            "{object}"
        );
        let hovered = client
            .request(
                "evaluate",
                json!({ "expression": "cart.owner", "frameId": frame, "context": "hover" }),
            )
            .await;
        assert_eq!(hovered["result"], "\"ann\"");
        for (expression, context, why) in [
            ("cart.owner.length()", "hover", "not called for a hover"),
            ("cart.items[0] / 0", "repl", "ArithmeticException"),
            ("cart.missing", "repl", "no field missing"),
            ("nothing(1)", "repl", "no method nothing"),
            ("cart.items[", "repl", "ended early"),
        ] {
            let seq = client
                .send(
                    "evaluate",
                    json!({ "expression": expression, "frameId": frame, "context": context }),
                )
                .await;
            let answer = client.answer(seq).await;
            assert_eq!(answer["success"], false, "{expression}: {answer}");
            assert!(
                answer["message"].as_str().unwrap().contains(why),
                "{expression}: {answer}"
            );
        }

        client.request("next", json!({ "threadId": thread })).await;
        assert_eq!(client.event("stopped").await["body"]["reason"], "step");
        let stack = client
            .request("stackTrace", json!({ "threadId": thread }))
            .await;
        assert_eq!(stack["stackFrames"][0]["line"], 11);
        client
            .request("continue", json!({ "threadId": thread }))
            .await;
        assert_eq!(
            client.event("stopped").await["body"]["reason"],
            "breakpoint"
        );
        let stack = client
            .request("stackTrace", json!({ "threadId": thread }))
            .await;
        assert_eq!(stack["stackFrames"][0]["line"], 20);
        let scopes = client
            .request(
                "scopes",
                json!({ "frameId": stack["stackFrames"][0]["id"] }),
            )
            .await;
        let locals = client
            .request(
                "variables",
                json!({ "variablesReference": scopes["scopes"][0]["variablesReference"] }),
            )
            .await;
        assert!(
            locals["variables"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["name"] == "t" && v["value"] == "10"),
            "{locals}"
        );
        client
            .request("continue", json!({ "threadId": thread }))
            .await;
        let exited = client.event("exited").await;
        assert_eq!(exited["body"]["exitCode"], 0);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !client.output().contains("total 10") && std::time::Instant::now() < deadline {
            client.gather(Duration::from_millis(500)).await;
        }
        assert!(
            client.output().contains("total 10 1"),
            "{}",
            client.output()
        );
        client.request("disconnect", json!({})).await;
        assert_eq!(running.await.unwrap(), Ok(()));
    }
}
