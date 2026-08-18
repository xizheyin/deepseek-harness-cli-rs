//! Deliberately faulty Phase 10 fixture used only by offline tests.

use std::{
    fs,
    io::{self, BufRead, Write as _},
    process::Command,
    time::Duration,
};

const MAX_RECORD_BYTES: u64 = 128 * 1024;

fn main() -> io::Result<()> {
    if std::env::var("DSH_PLUGIN_PROTOCOL").ok().as_deref() != Some("1")
        || std::env::var("DSH_PLUGIN_ID").ok().as_deref() != Some("fault-tools")
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "fault fixture environment is invalid",
        ));
    }
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "normal".to_owned());
    let marker = std::env::args().nth(2);
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let _host_hello = read_record(&mut input)?;
    if mode == "stall-hello" {
        let child = Command::new("/bin/sleep").arg("10").spawn()?;
        if let Some(marker) = marker {
            fs::write(marker, format!("{}\n", child.id()))?;
        }
        std::thread::sleep(Duration::from_secs(10));
        return Ok(());
    }
    write_stdout(concat!(
        r#"{"version":1,"type":"hello","plugin_id":"fault-tools","tools":[{"name":"fault_probe","description":"Exercise a bounded plugin fault","parameters":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"],"additionalProperties":false},"output":{"type":"string"}}]}"#,
        "\n"
    ).as_bytes())?;
    let call = read_record(&mut input)?;
    let call: serde_json::Value =
        serde_json::from_slice(&call).map_err(|_| invalid_data("call is invalid"))?;
    let id = call
        .get("id")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| invalid_data("call ID is invalid"))?;

    match mode.as_str() {
        "normal" => write_result(id, true, "ok"),
        "wrong-id" => write_result(id.saturating_add(1), true, "wrong"),
        "invalid-output" => {
            let line = format!(
                "{{\"version\":1,\"type\":\"result\",\"id\":{id},\"ok\":true,\"value\":7}}\n"
            );
            write_stdout(line.as_bytes())
        }
        "duplicate-result" => {
            write_result(id, true, "ok")?;
            write_result(id, true, "duplicate")
        }
        "crash-after-call" => {
            if let Some(marker) = marker {
                fs::write(marker, b"dispatched\n")?;
            }
            std::process::exit(17)
        }
        "extra-output" => {
            write_result(id, true, "ok")?;
            write_stdout(b"{}\n")
        }
        "cancel-settle" => {
            let _cancel = read_record(&mut input)?;
            let line = format!(
                "{{\"version\":1,\"type\":\"result\",\"id\":{id},\"ok\":false,\"error\":{{\"code\":\"CANCELLED\",\"message\":\"cancel observed\"}}}}\n"
            );
            write_stdout(line.as_bytes())
        }
        "ignore-cancel" => {
            let child = Command::new("/bin/sleep").arg("10").spawn()?;
            if let Some(marker) = marker {
                fs::write(marker, format!("{}\n", child.id()))?;
            }
            let _cancel = read_record(&mut input)?;
            std::thread::sleep(Duration::from_secs(10));
            Ok(())
        }
        "stall" => {
            std::thread::sleep(Duration::from_secs(10));
            Ok(())
        }
        "stdout-flood" => {
            write_stdout(&vec![b'x'; 128 * 1024 + 1])?;
            std::thread::sleep(Duration::from_secs(10));
            Ok(())
        }
        "stderr-flood" => {
            let mut stderr = io::stderr().lock();
            stderr.write_all(&vec![b'e'; 256 * 1024 + 1])?;
            stderr.flush()?;
            std::thread::sleep(Duration::from_secs(10));
            Ok(())
        }
        _ => Err(invalid_data("unknown fault mode")),
    }
}

fn read_record(input: &mut impl BufRead) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        let available = input.fill_buf()?;
        if available.is_empty() {
            return Err(invalid_data("record is missing or partial"));
        }
        let end = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(available.len());
        if (bytes.len() + end) as u64 > MAX_RECORD_BYTES {
            return Err(invalid_data("record is too large"));
        }
        let completed = available.get(end - 1) == Some(&b'\n');
        bytes.extend_from_slice(&available[..end]);
        input.consume(end);
        if completed {
            return Ok(bytes);
        }
    }
}

fn write_result(id: u64, ok: bool, value: &str) -> io::Result<()> {
    let line = serde_json::json!({
        "version":1,
        "type":"result",
        "id":id,
        "ok":ok,
        "value":value,
    });
    let mut bytes = serde_json::to_vec(&line).map_err(|_| invalid_data("encode failed"))?;
    bytes.push(b'\n');
    write_stdout(&bytes)
}

fn write_stdout(bytes: &[u8]) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(bytes)?;
    stdout.flush()
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
