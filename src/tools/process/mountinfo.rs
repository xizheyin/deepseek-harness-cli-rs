use std::io::{self, Read};

const MAX_RECORD_BYTES: usize = 65_536;

pub(super) fn validate<R: Read>(mut reader: R, expected_mount_id: u64) -> io::Result<()> {
    let mut chunk = [0_u8; 8_192];
    let mut record = Vec::with_capacity(512);
    let mut matches = 0_u8;
    loop {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            if !record.is_empty() {
                inspect_record(&record, expected_mount_id, &mut matches)?;
            }
            break;
        }
        for byte in &chunk[..count] {
            if *byte == b'\n' {
                inspect_record(&record, expected_mount_id, &mut matches)?;
                record.clear();
            } else {
                if record.len() == MAX_RECORD_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "mountinfo record exceeds the supported limit",
                    ));
                }
                record.push(*byte);
            }
        }
    }
    if matches == 1 {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "retained procfs mount record is missing or ambiguous",
        ))
    }
}

fn inspect_record(record: &[u8], expected: u64, matches: &mut u8) -> io::Result<()> {
    let fields: Vec<&[u8]> = record
        .split(|byte| *byte == b' ')
        .filter(|field| !field.is_empty())
        .collect();
    let Some(id) = fields.first().and_then(|field| parse_u64(field)) else {
        return Err(invalid_record());
    };
    if id != expected {
        return Ok(());
    }
    *matches = matches.checked_add(1).ok_or_else(invalid_record)?;
    if *matches != 1 || fields.len() < 10 {
        return Err(invalid_record());
    }
    let separator = fields
        .iter()
        .position(|field| *field == b"-")
        .ok_or_else(invalid_record)?;
    if separator < 6 || fields.get(separator + 1) != Some(&b"proc".as_slice()) {
        return Err(invalid_record());
    }
    let mount_options = fields.get(5).copied().ok_or_else(invalid_record)?;
    let super_options = fields
        .get(separator + 3)
        .copied()
        .ok_or_else(invalid_record)?;
    validate_options(mount_options)?;
    validate_options(super_options)
}

fn validate_options(options: &[u8]) -> io::Result<()> {
    for option in options.split(|byte| *byte == b',') {
        if option.starts_with(b"gid=") {
            return Err(invalid_record());
        }
        if option.starts_with(b"hidepid") && option != b"hidepid=0" && option != b"hidepid=off" {
            return Err(invalid_record());
        }
    }
    Ok(())
}

fn parse_u64(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() || bytes.iter().any(|byte| !byte.is_ascii_digit()) {
        return None;
    }
    bytes.iter().try_fold(0_u64, |value, byte| {
        value.checked_mul(10)?.checked_add(u64::from(*byte - b'0'))
    })
}

fn invalid_record() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "retained procfs mount record is unsupported",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(options: &str, super_options: &str) -> String {
        format!("31 24 0:27 / /proc rw,{options} - proc proc rw,{super_options}\n")
    }

    #[test]
    fn accepts_only_unhidden_proc_mounts() {
        validate(line("nosuid", "hidepid=0").as_bytes(), 31).unwrap();
        validate(line("nosuid", "hidepid=off").as_bytes(), 31).unwrap();
        validate(line("nosuid", "nodev").as_bytes(), 31).unwrap();
        assert!(validate(line("nosuid", "hidepid=2").as_bytes(), 31).is_err());
        assert!(validate(line("nosuid", "hidepid=ptraceable").as_bytes(), 31).is_err());
        assert!(validate(line("nosuid", "gid=1000").as_bytes(), 31).is_err());
    }

    #[test]
    fn requires_exactly_one_matching_mount_record() {
        assert!(validate(line("nosuid", "nodev").as_bytes(), 99).is_err());
        let duplicate = format!("{}{}", line("nosuid", "nodev"), line("nosuid", "nodev"));
        assert!(validate(duplicate.as_bytes(), 31).is_err());
    }

    #[test]
    fn rejects_a_record_one_byte_past_the_limit() {
        let mut overlong = vec![b'x'; MAX_RECORD_BYTES + 1];
        overlong.push(b'\n');
        assert!(validate(overlong.as_slice(), 31).is_err());
    }
}
