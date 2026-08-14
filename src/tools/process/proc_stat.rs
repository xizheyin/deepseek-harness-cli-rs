#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ProcStat {
    pub(super) pid: u32,
    pub(super) state: u8,
    pub(super) parent: u32,
    pub(super) process_group: u32,
    pub(super) session: u32,
    pub(super) threads: u32,
}

pub(super) fn parse(bytes: &[u8]) -> Option<ProcStat> {
    if bytes.is_empty() || bytes.len() > 4_096 {
        return None;
    }
    let first_space = bytes.iter().position(|byte| *byte == b' ')?;
    let pid = parse_u32(&bytes[..first_space])?;
    if bytes.get(first_space + 1) != Some(&b'(') {
        return None;
    }
    let close = bytes.windows(2).rposition(|pair| pair == b") ")?;
    if close <= first_space + 1 {
        return None;
    }
    let fields: Vec<&[u8]> = bytes[close + 2..]
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty())
        .collect();
    if fields.len() <= 17 || fields[0].len() != 1 {
        return None;
    }
    Some(ProcStat {
        pid,
        state: fields[0][0],
        parent: parse_u32(fields[1])?,
        process_group: parse_u32(fields[2])?,
        session: parse_u32(fields[3])?,
        threads: parse_u32(fields[17])?,
    })
}

fn parse_u32(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || bytes.iter().any(|byte| !byte.is_ascii_digit()) {
        return None;
    }
    bytes.iter().try_fold(0_u32, |value, byte| {
        value.checked_mul(10)?.checked_add(u32::from(*byte - b'0'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str, state: &str, threads: &str) -> Vec<u8> {
        format!("42 ({name}) {state} 7 42 42 0 0 0 0 0 0 0 0 0 0 0 0 0 {threads} 0\n").into_bytes()
    }

    #[test]
    fn parses_from_the_final_closing_parenthesis() {
        let stat = parse(&record("a ) command with spaces", "Z", "1")).unwrap();
        assert_eq!(
            stat,
            ProcStat {
                pid: 42,
                state: b'Z',
                parent: 7,
                process_group: 42,
                session: 42,
                threads: 1,
            }
        );
    }

    #[test]
    fn rejects_missing_or_overflowing_thread_counts() {
        let mut missing = record("bash", "Z", "1");
        missing.truncate(missing.windows(4).position(|w| w == b" 1 0").unwrap());
        assert_eq!(parse(&missing), None);
        assert_eq!(parse(&record("bash", "Z", "4294967296")), None);
    }

    #[test]
    fn accepts_the_exact_record_limit_and_rejects_one_over() {
        let mut exact = record("bash", "Z", "1");
        exact.resize(4_096, b' ');
        assert!(parse(&exact).is_some());
        exact.push(b' ');
        assert_eq!(parse(&exact), None);
    }
}
