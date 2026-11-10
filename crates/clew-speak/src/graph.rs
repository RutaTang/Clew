//! A model's graph made runnable by the ONNX Runtime here: what it is
//! asked to do that this runtime's CPU code cannot, done instead by
//! operators it has.
//!
//! The one change made: an attention node (`GroupQueryAttention`) given
//! its queries' and keys' RMS-normalisation weights to apply itself — the
//! "QK norm" of Qwen3's layers, which ONNX Runtime fuses into attention on
//! the GPU only — gets queries and keys already normalised instead, by a
//! `SimplifiedLayerNormalization` over each head, as the runtime's model
//! builder writes them for the CPU. The arithmetic is the same.
//!
//! The model is edited as protobuf on the wire: the fields of a message
//! that are not changed are kept byte for byte, so nothing this does not
//! know of — a field of a newer ONNX, the external weights' locations — is
//! lost.

/// One field of a protobuf message, as it was on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Field<'a> {
    number: u32,
    value: Value<'a>,
    /// The whole field — its key and its value — as written.
    raw: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Value<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Fixed32(u32),
}

/// What a malformed model is told as.
fn malformed(what: &str) -> String {
    format!("the model's graph is malformed: {what}")
}

fn varint(buf: &[u8], at: &mut usize) -> Result<u64, String> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let &byte = buf
            .get(*at)
            .ok_or_else(|| malformed("a number runs past its end"))?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(malformed("a number is too long"))
}

/// The fields of the message `buf`, in their order.
fn fields(buf: &[u8]) -> Result<Vec<Field<'_>>, String> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < buf.len() {
        let start = at;
        let key = varint(buf, &mut at)?;
        let number = u32::try_from(key >> 3).map_err(|_| malformed("a field number"))?;
        let take = |at: &mut usize, n: usize| -> Result<&[u8], String> {
            let end = at
                .checked_add(n)
                .filter(|&end| end <= buf.len())
                .ok_or_else(|| malformed("a field runs past its end"))?;
            let bytes = &buf[*at..end];
            *at = end;
            Ok(bytes)
        };
        let value = match key & 7 {
            0 => Value::Varint(varint(buf, &mut at)?),
            1 => Value::Fixed64(u64::from_le_bytes(take(&mut at, 8)?.try_into().unwrap())),
            2 => {
                let len =
                    usize::try_from(varint(buf, &mut at)?).map_err(|_| malformed("a length"))?;
                Value::Bytes(take(&mut at, len)?)
            }
            5 => Value::Fixed32(u32::from_le_bytes(take(&mut at, 4)?.try_into().unwrap())),
            wire => return Err(malformed(&format!("wire type {wire}"))),
        };
        out.push(Field {
            number,
            value,
            raw: &buf[start..at],
        });
    }
    Ok(out)
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn put_bytes(out: &mut Vec<u8>, number: u32, bytes: &[u8]) {
    put_varint(out, u64::from(number) << 3 | 2);
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn put_int(out: &mut Vec<u8>, number: u32, value: i64) {
    put_varint(out, u64::from(number) << 3);
    put_varint(out, value as u64);
}

fn put_float(out: &mut Vec<u8>, number: u32, value: f32) {
    put_varint(out, u64::from(number) << 3 | 5);
    out.extend_from_slice(&value.to_le_bytes());
}

fn bytes_of<'a>(field: &Field<'a>) -> Option<&'a [u8]> {
    match field.value {
        Value::Bytes(b) => Some(b),
        _ => None,
    }
}

fn text_of<'a>(field: &Field<'a>) -> Result<&'a str, String> {
    std::str::from_utf8(bytes_of(field).ok_or_else(|| malformed("a name is not text"))?)
        .map_err(|_| malformed("a name is not UTF-8"))
}

// The fields of ONNX's messages used here (onnx.proto).
const MODEL_GRAPH: u32 = 7;
const GRAPH_NODE: u32 = 1;
const GRAPH_INITIALIZER: u32 = 5;
const NODE_INPUT: u32 = 1;
const NODE_OUTPUT: u32 = 2;
const NODE_NAME: u32 = 3;
const NODE_OP_TYPE: u32 = 4;
const NODE_ATTRIBUTE: u32 = 5;
const ATTRIBUTE_NAME: u32 = 1;
const ATTRIBUTE_F: u32 = 2;
const ATTRIBUTE_I: u32 = 3;
const ATTRIBUTE_TYPE: u32 = 20;
const ATTRIBUTE_FLOAT: i64 = 1;
const ATTRIBUTE_INT: i64 = 2;
const TENSOR_DIMS: u32 = 1;
const TENSOR_DATA_TYPE: u32 = 2;
const TENSOR_NAME: u32 = 8;
const TENSOR_RAW_DATA: u32 = 9;
const TENSOR_INT64: i64 = 7;

/// Where in an attention node's inputs its queries, its keys, and the
/// weights of their normalisation are.
const QUERY: usize = 0;
const KEY: usize = 1;
const Q_NORM: usize = 14;
const K_NORM: usize = 15;
/// The inputs an attention node keeps once its normalisation is done
/// before it: queries, keys, values, the past keys and values, the
/// sequence lengths, the rotary cosines and sines.
const KEPT_INPUTS: usize = 9;

/// A node, written out.
fn node(op: &str, name: &str, inputs: &[&str], output: &str, attributes: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for input in inputs {
        put_bytes(&mut out, NODE_INPUT, input.as_bytes());
    }
    put_bytes(&mut out, NODE_OUTPUT, output.as_bytes());
    put_bytes(&mut out, NODE_NAME, name.as_bytes());
    put_bytes(&mut out, NODE_OP_TYPE, op.as_bytes());
    for attribute in attributes {
        put_bytes(&mut out, NODE_ATTRIBUTE, attribute);
    }
    out
}

fn int_attribute(name: &str, value: i64) -> Vec<u8> {
    let mut out = Vec::new();
    put_bytes(&mut out, ATTRIBUTE_NAME, name.as_bytes());
    put_int(&mut out, ATTRIBUTE_I, value);
    put_int(&mut out, ATTRIBUTE_TYPE, ATTRIBUTE_INT);
    out
}

fn float_attribute(name: &str, value: f32) -> Vec<u8> {
    let mut out = Vec::new();
    put_bytes(&mut out, ATTRIBUTE_NAME, name.as_bytes());
    put_float(&mut out, ATTRIBUTE_F, value);
    put_int(&mut out, ATTRIBUTE_TYPE, ATTRIBUTE_FLOAT);
    out
}

/// A constant of 64-bit integers, as a graph's initializer.
fn int64_initializer(name: &str, values: &[i64]) -> Vec<u8> {
    let mut out = Vec::new();
    put_int(&mut out, TENSOR_DIMS, values.len() as i64);
    put_int(&mut out, TENSOR_DATA_TYPE, TENSOR_INT64);
    put_bytes(&mut out, TENSOR_NAME, name.as_bytes());
    let raw: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
    put_bytes(&mut out, TENSOR_RAW_DATA, &raw);
    out
}

/// An attention node's attribute `name`, as a float or an integer.
fn attribute(fields: &[Field<'_>], name: &str) -> Result<Option<Value<'static>>, String> {
    for field in fields.iter().filter(|f| f.number == NODE_ATTRIBUTE) {
        let parts = fields_of(field)?;
        let named = parts
            .iter()
            .find(|p| p.number == ATTRIBUTE_NAME)
            .map(text_of)
            .transpose()?;
        if named != Some(name) {
            continue;
        }
        for part in &parts {
            match (part.number, part.value) {
                (ATTRIBUTE_F, Value::Fixed32(bits)) => return Ok(Some(Value::Fixed32(bits))),
                (ATTRIBUTE_I, Value::Varint(v)) => return Ok(Some(Value::Varint(v))),
                _ => {}
            }
        }
    }
    Ok(None)
}

fn fields_of<'a>(field: &Field<'a>) -> Result<Vec<Field<'a>>, String> {
    fields(bytes_of(field).ok_or_else(|| malformed("a message is not one"))?)
}

/// The names of the shapes each attention node's heads are cut into.
fn heads_shape(heads: u64) -> String {
    format!("clew/qk_norm/heads_{heads}")
}

/// `model` (the bytes of its `.onnx`), its attention nodes' QK norm done
/// before them by nodes the CPU runs ([module docs](self)), and how many
/// were changed. A model without such nodes comes back as it was.
pub fn unfuse_qk_norm(model: &[u8]) -> Result<(Vec<u8>, usize), String> {
    let top = fields(model)?;
    let mut out = Vec::with_capacity(model.len() + 64 * 1024);
    let mut changed = 0;
    for field in &top {
        if field.number != MODEL_GRAPH {
            out.extend_from_slice(field.raw);
            continue;
        }
        let (graph, n) = graph_unfused(bytes_of(field).ok_or_else(|| malformed("the graph"))?)?;
        changed += n;
        put_bytes(&mut out, MODEL_GRAPH, &graph);
    }
    Ok((out, changed))
}

fn graph_unfused(graph: &[u8]) -> Result<(Vec<u8>, usize), String> {
    let mut out = Vec::with_capacity(graph.len() + 64 * 1024);
    let mut changed = 0;
    let mut shapes = std::collections::BTreeSet::new();
    for field in fields(graph)? {
        if field.number != GRAPH_NODE {
            out.extend_from_slice(field.raw);
            continue;
        }
        let parts = fields_of(&field)?;
        let op = parts
            .iter()
            .find(|p| p.number == NODE_OP_TYPE)
            .map(text_of)
            .transpose()?;
        let inputs: Vec<&str> = parts
            .iter()
            .filter(|p| p.number == NODE_INPUT)
            .map(text_of)
            .collect::<Result<_, _>>()?;
        let fused = op == Some("GroupQueryAttention")
            && inputs.len() > K_NORM
            && !inputs[Q_NORM].is_empty()
            && !inputs[K_NORM].is_empty();
        if !fused {
            out.extend_from_slice(field.raw);
            continue;
        }
        let name = parts
            .iter()
            .find(|p| p.number == NODE_NAME)
            .map(text_of)
            .transpose()?
            .unwrap_or("attention");
        let heads = |which: &str| match attribute(&parts, which) {
            Ok(Some(Value::Varint(n))) if n > 0 => Ok(n),
            Ok(_) => Err(malformed(&format!("an attention node without {which}"))),
            Err(e) => Err(e),
        };
        let (q_heads, kv_heads) = (heads("num_heads")?, heads("kv_num_heads")?);
        let epsilon = match attribute(&parts, "qk_norm_epsilon")? {
            Some(Value::Fixed32(bits)) => f32::from_bits(bits),
            _ => 1e-6,
        };
        // Each of queries and keys: cut into its heads, each head
        // normalised, put back as it was.
        let mut normalised = [String::new(), String::new()];
        for (slot, (input, weight, heads, tag)) in [
            (inputs[QUERY], inputs[Q_NORM], q_heads, "q"),
            (inputs[KEY], inputs[K_NORM], kv_heads, "k"),
        ]
        .into_iter()
        .enumerate()
        {
            let base = format!("{name}/clew_{tag}_norm");
            let (shape, heads_of, norm, back) = (
                format!("{base}/shape"),
                format!("{base}/heads"),
                format!("{base}/norm"),
                format!("{base}/out"),
            );
            shapes.insert(heads);
            for node in [
                node("Shape", &shape, &[input], &shape, &[]),
                node(
                    "Reshape",
                    &heads_of,
                    &[input, &heads_shape(heads)],
                    &heads_of,
                    &[],
                ),
                node(
                    "SimplifiedLayerNormalization",
                    &norm,
                    &[&heads_of, weight],
                    &norm,
                    &[
                        int_attribute("axis", -1),
                        float_attribute("epsilon", epsilon),
                        int_attribute("stash_type", 1),
                    ],
                ),
                node("Reshape", &back, &[&norm, &shape], &back, &[]),
            ] {
                put_bytes(&mut out, GRAPH_NODE, &node);
            }
            normalised[slot] = back;
        }
        // The node itself: given what is normalised, and nothing to
        // normalise.
        let mut rewritten = Vec::with_capacity(field.raw.len());
        let mut index = 0;
        for part in &parts {
            match part.number {
                NODE_INPUT => {
                    match index {
                        QUERY | KEY => {
                            put_bytes(&mut rewritten, NODE_INPUT, normalised[index].as_bytes())
                        }
                        i if i < KEPT_INPUTS => rewritten.extend_from_slice(part.raw),
                        _ => {}
                    }
                    index += 1;
                }
                NODE_ATTRIBUTE => {
                    let named = fields_of(part)?
                        .iter()
                        .find(|p| p.number == ATTRIBUTE_NAME)
                        .map(text_of)
                        .transpose()?;
                    if named != Some("qk_norm_epsilon") {
                        rewritten.extend_from_slice(part.raw);
                    }
                }
                _ => rewritten.extend_from_slice(part.raw),
            }
        }
        put_bytes(&mut out, GRAPH_NODE, &rewritten);
        changed += 1;
    }
    for heads in shapes {
        // Batch and sequence kept (0), the heads, and each head's width.
        let shape = int64_initializer(&heads_shape(heads), &[0, 0, heads as i64, -1]);
        put_bytes(&mut out, GRAPH_INITIALIZER, &shape);
    }
    Ok((out, changed))
}

/// The nodes of `model`'s graph, one line each — op, inputs, outputs — for
/// a test to read.
#[cfg(test)]
fn describe(model: &[u8]) -> Result<String, String> {
    use std::fmt::Write as _;
    let mut out = String::new();
    for field in fields(model)?.iter().filter(|f| f.number == MODEL_GRAPH) {
        for node in fields_of(field)?.iter().filter(|f| f.number == GRAPH_NODE) {
            let parts = fields_of(node)?;
            let all = |n: u32| -> Result<Vec<&str>, String> {
                parts
                    .iter()
                    .filter(|p| p.number == n)
                    .map(text_of)
                    .collect()
            };
            let attributes: Vec<String> = parts
                .iter()
                .filter(|p| p.number == NODE_ATTRIBUTE)
                .map(|p| {
                    let a = fields_of(p)?;
                    let name = a
                        .iter()
                        .find(|f| f.number == ATTRIBUTE_NAME)
                        .map(text_of)
                        .transpose()?
                        .unwrap_or("");
                    let value = a
                        .iter()
                        .find_map(|f| match (f.number, f.value) {
                            (ATTRIBUTE_F, Value::Fixed32(b)) => Some(f32::from_bits(b).to_string()),
                            (ATTRIBUTE_I, Value::Varint(v)) => Some((v as i64).to_string()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    Ok(format!("{name}={value}"))
                })
                .collect::<Result<_, String>>()?;
            writeln!(
                out,
                "{} [{}] -> [{}] {{{}}}",
                all(NODE_OP_TYPE)?.join(""),
                all(NODE_INPUT)?.join(", "),
                all(NODE_OUTPUT)?.join(", "),
                attributes.join(", ")
            )
            .map_err(|e| e.to_string())?;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A model of one attention node with its QK norm fused, between two
    /// other nodes, with fields around them this does not know.
    fn fused_model() -> Vec<u8> {
        let mut attention = Vec::new();
        for input in [
            "q", "k", "v", "past.k", "past.v", "seqlens", "total", "cos", "sin", "", "", "", "",
            "", "q_norm.w", "k_norm.w",
        ] {
            put_bytes(&mut attention, NODE_INPUT, input.as_bytes());
        }
        put_bytes(&mut attention, NODE_OUTPUT, b"attn");
        put_bytes(&mut attention, NODE_OUTPUT, b"present.k");
        put_bytes(&mut attention, NODE_NAME, b"/layers.0/attn/GQA");
        put_bytes(&mut attention, NODE_OP_TYPE, b"GroupQueryAttention");
        put_bytes(&mut attention, 7, b"com.microsoft");
        put_bytes(
            &mut attention,
            NODE_ATTRIBUTE,
            &int_attribute("num_heads", 16),
        );
        put_bytes(
            &mut attention,
            NODE_ATTRIBUTE,
            &int_attribute("kv_num_heads", 8),
        );
        put_bytes(
            &mut attention,
            NODE_ATTRIBUTE,
            &float_attribute("qk_norm_epsilon", 1e-6),
        );
        put_bytes(
            &mut attention,
            NODE_ATTRIBUTE,
            &int_attribute("do_rotary", 1),
        );
        let mut graph = Vec::new();
        put_bytes(
            &mut graph,
            GRAPH_NODE,
            &node("Split", "split", &["x"], "q", &[]),
        );
        put_bytes(&mut graph, GRAPH_NODE, &attention);
        put_bytes(
            &mut graph,
            GRAPH_NODE,
            &node("MatMul", "o", &["attn", "w"], "y", &[]),
        );
        put_bytes(&mut graph, 2, b"the graph's name");
        put_bytes(
            &mut graph,
            GRAPH_INITIALIZER,
            &int64_initializer("w", &[1, 2]),
        );
        let mut model = Vec::new();
        put_int(&mut model, 1, 10); // ir_version
        put_bytes(&mut model, 99, b"a field of a newer ONNX");
        put_bytes(&mut model, MODEL_GRAPH, &graph);
        put_bytes(&mut model, 8, b"\x0a\x00\x10\x16"); // opset_import
        model
    }

    /// Read, every field is as it was written; written back, every byte.
    #[test]
    fn fields_read_and_write_back_as_they_were() {
        let model = fused_model();
        let top = fields(&model).unwrap();
        assert_eq!(
            top.iter().map(|f| f.number).collect::<Vec<_>>(),
            [1, 99, MODEL_GRAPH, 8]
        );
        assert_eq!(top[0].value, Value::Varint(10));
        let back: Vec<u8> = top.iter().flat_map(|f| f.raw.iter().copied()).collect();
        assert_eq!(back, model);
        // Fixed widths too.
        let mut buf = Vec::new();
        put_float(&mut buf, 2, 0.5);
        buf.extend_from_slice(&[0x09, 1, 0, 0, 0, 0, 0, 0, 0]); // field 1, fixed64
        let read = fields(&buf).unwrap();
        assert_eq!(read[0].value, Value::Fixed32(0.5f32.to_bits()));
        assert_eq!(read[1].value, Value::Fixed64(1));
    }

    /// The attention node gets its queries and keys normalised before it,
    /// head by head, with the epsilon it was given — and keeps everything
    /// else it had; the nodes around it, and the fields this does not know,
    /// are untouched.
    #[test]
    fn the_qk_norm_is_done_before_attention() {
        let model = fused_model();
        let (patched, changed) = unfuse_qk_norm(&model).unwrap();
        assert_eq!(changed, 1);
        let base = "/layers.0/attn/GQA";
        let expected = format!(
            "Split [x] -> [q] {{}}\n\
             Shape [q] -> [{base}/clew_q_norm/shape] {{}}\n\
             Reshape [q, clew/qk_norm/heads_16] -> [{base}/clew_q_norm/heads] {{}}\n\
             SimplifiedLayerNormalization [{base}/clew_q_norm/heads, q_norm.w] -> \
             [{base}/clew_q_norm/norm] {{axis=-1, epsilon=0.000001, stash_type=1}}\n\
             Reshape [{base}/clew_q_norm/norm, {base}/clew_q_norm/shape] -> [{base}/clew_q_norm/out] {{}}\n\
             Shape [k] -> [{base}/clew_k_norm/shape] {{}}\n\
             Reshape [k, clew/qk_norm/heads_8] -> [{base}/clew_k_norm/heads] {{}}\n\
             SimplifiedLayerNormalization [{base}/clew_k_norm/heads, k_norm.w] -> \
             [{base}/clew_k_norm/norm] {{axis=-1, epsilon=0.000001, stash_type=1}}\n\
             Reshape [{base}/clew_k_norm/norm, {base}/clew_k_norm/shape] -> [{base}/clew_k_norm/out] {{}}\n\
             GroupQueryAttention [{base}/clew_q_norm/out, {base}/clew_k_norm/out, v, past.k, past.v, \
             seqlens, total, cos, sin] -> [attn, present.k] {{num_heads=16, kv_num_heads=8, do_rotary=1}}\n\
             MatMul [attn, w] -> [y] {{}}\n"
        );
        assert_eq!(describe(&patched).unwrap(), expected);
        // The fields around the graph, as they were.
        let top = fields(&patched).unwrap();
        assert_eq!(top[1].raw, fields(&model).unwrap()[1].raw);
        assert_eq!(top[3].raw, fields(&model).unwrap()[3].raw);
        // The graph's own: its name kept, the head shapes added once each.
        let graph = fields_of(&top[2]).unwrap();
        assert!(
            graph
                .iter()
                .any(|f| f.number == 2 && bytes_of(f) == Some(b"the graph's name"))
        );
        let shapes: Vec<(String, Vec<i64>)> = graph
            .iter()
            .filter(|f| f.number == GRAPH_INITIALIZER)
            .map(|f| {
                let t = fields_of(f).unwrap();
                let name = t
                    .iter()
                    .find(|p| p.number == TENSOR_NAME)
                    .map(|p| text_of(p).unwrap().to_string())
                    .unwrap();
                let raw = t
                    .iter()
                    .find(|p| p.number == TENSOR_RAW_DATA)
                    .and_then(bytes_of)
                    .unwrap();
                (
                    name,
                    raw.chunks(8)
                        .map(|c| i64::from_le_bytes(c.try_into().unwrap()))
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            shapes,
            [
                ("w".to_string(), vec![1, 2]),
                ("clew/qk_norm/heads_8".to_string(), vec![0, 0, 8, -1]),
                ("clew/qk_norm/heads_16".to_string(), vec![0, 0, 16, -1]),
            ]
        );
        // Done again, there is nothing left to do.
        let (again, changed) = unfuse_qk_norm(&patched).unwrap();
        assert_eq!(changed, 0);
        assert_eq!(again, patched);
    }

    /// An attention node without the fused norm — the code predictor's —
    /// and a model with none are left as they were.
    #[test]
    fn a_model_without_the_fused_norm_is_left_alone() {
        let mut attention = Vec::new();
        for input in ["q", "k", "v"] {
            put_bytes(&mut attention, NODE_INPUT, input.as_bytes());
        }
        put_bytes(&mut attention, NODE_OP_TYPE, b"GroupQueryAttention");
        let mut graph = Vec::new();
        put_bytes(&mut graph, GRAPH_NODE, &attention);
        let mut model = Vec::new();
        put_bytes(&mut model, MODEL_GRAPH, &graph);
        assert_eq!(unfuse_qk_norm(&model).unwrap(), (model.clone(), 0));
        assert_eq!(unfuse_qk_norm(&[]).unwrap(), (Vec::new(), 0));
    }

    /// A model cut short, or with what protobuf has no wire type for, is
    /// refused with a reason — never read past its end.
    #[test]
    fn a_malformed_model_is_refused() {
        let model = fused_model();
        for cut in [1, 5, model.len() / 2, model.len() - 1] {
            assert!(unfuse_qk_norm(&model[..cut]).is_err(), "cut at {cut}");
        }
        assert!(unfuse_qk_norm(&[0x0b]).unwrap_err().contains("wire type 3"));
        assert!(
            unfuse_qk_norm(&[
                0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01
            ])
            .is_err()
        );
        // An attention node missing its head counts.
        let mut attention = Vec::new();
        for input in [
            "q", "k", "v", "", "", "", "", "", "", "", "", "", "", "", "qn", "kn",
        ] {
            put_bytes(&mut attention, NODE_INPUT, input.as_bytes());
        }
        put_bytes(&mut attention, NODE_OP_TYPE, b"GroupQueryAttention");
        let mut graph = Vec::new();
        put_bytes(&mut graph, GRAPH_NODE, &attention);
        let mut model = Vec::new();
        put_bytes(&mut model, MODEL_GRAPH, &graph);
        assert!(unfuse_qk_norm(&model).unwrap_err().contains("num_heads"));
    }
}
