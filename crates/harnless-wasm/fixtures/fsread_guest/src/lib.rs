#[allow(warnings)]
mod bindings;

use bindings::Guest;
use bindings::wasi::filesystem::preopens;
use bindings::wasi::filesystem::types;

/// The tool argument payload is raw JSON; pull `path` out of it without a
/// JSON dependency (the fixtures keep the guest body minimal).
fn arg_path(input: &str) -> String {
    let after = input
        .split_once("\"path\"")
        .map(|(_, r)| r)
        .and_then(|r| r.split_once(':').map(|(_, v)| v));
    let value = match after {
        Some(v) => v,
        None => return "note.txt".to_string(),
    };
    let value = value.trim();
    let value = value.trim_start_matches('"');
    let end = value.find('"').unwrap_or(value.len());
    value[..end].to_string()
}

/// Escape `s` for a JSON string literal.
fn json_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Open `path` under the first preopen (the host's `/scope`) and read to EOF.
fn open_and_read(path: &str) -> Result<Vec<u8>, String> {
    let dirs = preopens::get_directories();
    let (base, _) = dirs.into_iter().next().ok_or("no preopen")?;
    let file = base
        .open_at(
            types::PathFlags::empty(),
            path,
            types::OpenFlags::empty(),
            types::DescriptorFlags::READ,
        )
        .map_err(|e| format!("{e:?}"))?;
    let stream = file.read_via_stream(0).map_err(|e| format!("{e:?}"))?;
    let mut out = Vec::new();
    loop {
        match stream.blocking_read(64) {
            Ok(chunk) if !chunk.is_empty() => out.extend_from_slice(&chunk),
            // EOF surfaces as `Closed` once the stream is drained.
            Ok(_) | Err(_) => break,
        }
    }
    Ok(out)
}

struct Component;

impl Guest for Component {
    fn descriptor() -> String {
        "{\"name\":\"fsreal\",\"tools\":[{\"name\":\"read\",\"schema\":{\"type\":\"object\"},\"output\":\"json\",\"serialized\":false}]}".to_string()
    }
    fn call_read(input: String) -> String {
        let path = arg_path(&input);
        match open_and_read(&path) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                format!("{{\"content\":\"{}\"}}", json_escape(&text))
            }
            Err(e) => format!("{{\"error\":\"{}\"}}", json_escape(&e)),
        }
    }
    fn call_escape(input: String) -> String {
        let path = arg_path(&input);
        match open_and_read(&path) {
            Ok(_) => "{\"opened\":true}".to_string(),
            Err(e) => format!("{{\"error\":\"{}\"}}", json_escape(&e)),
        }
    }
}

bindings::export!(Component with_types_in bindings);
