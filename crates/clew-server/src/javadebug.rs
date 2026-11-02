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
    /// A frame's locals (and `this`).
    Locals {
        thread: u64,
        frame: u64,
        location: Location,
    },
    /// An object's fields or an array's elements.
    Object { object: u64, array: bool },
}

#[derive(Default)]
struct State {
    /// Each source file's breakpoints, and the classes they are for.
    breakpoints: HashMap<String, (String, Vec<Wanted>)>,
    /// The DAP frame ids handed out since the program last stopped.
    frames: Vec<(u64, u64, Location)>,
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
                Ok(vm) => vm
                    .resume()
                    .await
                    .map(|()| json!({ "allThreadsContinued": true })),
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
        let mut command = tokio::process::Command::new(&self.java);
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
        let mut command = tokio::process::Command::new(&self.javac);
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
        vm.resume().await?;
        Ok(json!({}))
    }

    async fn stack_trace(&self, args: &Value) -> Result<Value, String> {
        let vm = self.vm()?;
        let thread = args["threadId"].as_u64().ok_or("which thread?")?;
        let frames = vm.frames(thread).await?;
        let mut shown = Vec::new();
        for (frame, location) in frames {
            let id = {
                let mut state = self.state.lock().await;
                state.frames.push((thread, frame, location));
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
        let (thread, frame, location) = *id
            .checked_sub(1)
            .and_then(|i| state.frames.get(i))
            .ok_or("no such frame")?;
        state.refs.push(VarRef::Locals {
            thread,
            frame,
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
                frame,
                location,
            } => locals(&vm, thread, frame, location).await?,
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

    /// A variable, and the fields and elements along a path from it.
    async fn evaluate(&self, args: &Value) -> Result<Value, String> {
        let expression = args["expression"].as_str().unwrap_or("").trim().to_string();
        let path = parse_path(&expression).ok_or(
            "JDWP evaluates no expressions — name a variable, or a field or element of one",
        )?;
        let id = args["frameId"].as_u64().unwrap_or(0) as usize;
        let (thread, frame, location) = {
            let state = self.state.lock().await;
            *id.checked_sub(1)
                .and_then(|i| state.frames.get(i))
                .ok_or("evaluate in a stopped thread's frame")?
        };
        let vm = self.vm()?;
        let scope = locals(&vm, thread, frame, location).await?;
        let mut steps = path.into_iter();
        let first = steps.next().ok_or("nothing to evaluate")?;
        let PathStep::Name(first) = first else {
            return Err("start with a variable's name".into());
        };
        let mut value = match scope.iter().find(|(n, _)| *n == first) {
            Some((_, v)) => *v,
            None => {
                // A field of `this`.
                let this = scope.iter().find(|(n, _)| n == "this").map(|(_, v)| *v);
                let this = this.ok_or_else(|| format!("no variable {first} here"))?;
                field_of(&vm, this, &first).await?
            }
        };
        for step in steps {
            value = match step {
                PathStep::Name(name) => field_of(&vm, value, &name).await?,
                PathStep::Index(i) => element_of(&vm, value, i).await?,
            };
        }
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
        b'F' => f32::from_bits(value.bits as u32).to_string(),
        b'D' => f64::from_bits(value.bits).to_string(),
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

#[derive(Debug, PartialEq)]
enum PathStep {
    Name(String),
    Index(usize),
}

/// `order.items[0].price` as its steps; `None` for anything else — a call,
/// an operator.
fn parse_path(expression: &str) -> Option<Vec<PathStep>> {
    let mut steps = Vec::new();
    let mut rest = expression;
    loop {
        let end = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
            .unwrap_or(rest.len());
        let name = &rest[..end];
        if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
            return None;
        }
        steps.push(PathStep::Name(name.to_string()));
        rest = &rest[end..];
        while let Some(after) = rest.strip_prefix('[') {
            let close = after.find(']')?;
            steps.push(PathStep::Index(after[..close].trim().parse().ok()?));
            rest = &after[close + 1..];
        }
        match rest.strip_prefix('.') {
            Some(after) => rest = after,
            None if rest.is_empty() => return Some(steps),
            None => return None,
        }
    }
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
        assert_eq!(
            parse_path("order.items[0].price"),
            Some(vec![
                PathStep::Name("order".into()),
                PathStep::Name("items".into()),
                PathStep::Index(0),
                PathStep::Name("price".into())
            ])
        );
        for not_a_path in ["a + b", "f(x)", "items[i]", "", "1x", "a."] {
            assert_eq!(parse_path(not_a_path), None, "{not_a_path}");
        }
        assert_eq!(
            package_of("// x\npackage demo.shop;\n\nclass A {}"),
            Some("demo.shop".into())
        );
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
            "package demo;\n\npublic class Main {\n    static class Cart {\n        String owner = \"ann\";\n        int[] items = {2, 3};\n    }\n\n    static int total(Cart cart) {\n        int sum = 0;\n        for (int i : cart.items) {\n            sum += i;\n        }\n        return sum * 2;\n    }\n\n    public static void main(String[] args) {\n        Cart cart = new Cart();\n        int t = total(cart);\n        System.out.println(\"total \" + t + \" \" + args.length);\n    }\n}\n",
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
        let read = client
            .request(
                "evaluate",
                json!({ "expression": "cart.items[1]", "frameId": frames[0]["id"] }),
            )
            .await;
        assert_eq!(read["result"], "3");
        let refused = client
            .send(
                "evaluate",
                json!({ "expression": "cart.items[0] + 1", "frameId": frames[0]["id"] }),
            )
            .await;
        assert_eq!(client.answer(refused).await["success"], false);

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
