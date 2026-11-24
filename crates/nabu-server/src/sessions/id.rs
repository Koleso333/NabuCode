use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

// Same scheme as the UI's `Identifier`: prefix, 6 bytes of time and counter, random tail.
// Ascending ids sort oldest first, descending ones newest first.

static LAST: Mutex<(u64, u64)> = Mutex::new((0, 0));

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn ascending(prefix: &str) -> String {
    create(prefix, false)
}

pub fn descending(prefix: &str) -> String {
    create(prefix, true)
}

/// An ascending id that sorts after `other`, even when `other` came from a client clock ahead of ours.
pub fn ascending_after(prefix: &str, other: &str) -> String {
    let after = other
        .split_once('_')
        .and_then(|(_, rest)| rest.get(..12))
        .and_then(|hex| u64::from_str_radix(hex, 16).ok());
    let stamp = next_stamp();
    let stamp = match after {
        Some(after) if stamp & LOW <= after && after < LOW => after + 1,
        _ => stamp,
    };
    format!("{prefix}_{}{}", hex(stamp), random(14))
}

fn create(prefix: &str, descending: bool) -> String {
    let stamp = next_stamp();
    let stamp = if descending { !stamp } else { stamp };
    format!("{prefix}_{}{}", hex(stamp), random(14))
}

const LOW: u64 = (1 << 48) - 1;

fn next_stamp() -> u64 {
    let mut last = LAST.lock().unwrap();
    let time = now();
    if time != last.0 {
        *last = (time, 0);
    }
    last.1 += 1;
    time * 0x1000 + last.1
}

// Low six bytes only, as the UI does.
fn hex(stamp: u64) -> String {
    (0..6)
        .map(|i| format!("{:02x}", (stamp >> (40 - 8 * i)) & 0xff))
        .collect()
}

fn random(len: usize) -> String {
    const CHARS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut bytes = vec![0u8; len];
    getrandom::fill(&mut bytes).expect("os random");
    bytes
        .iter()
        .map(|b| CHARS[*b as usize % CHARS.len()] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order() {
        let (a, b) = (ascending("msg"), ascending("msg"));
        assert!(a < b);
        let (a, b) = (descending("ses"), descending("ses"));
        assert!(a > b);
        let ahead = format!("msg_{}zzzz", hex((now() + 5) * 0x1000));
        assert!(ascending_after("msg", &ahead) > ahead);
        // An id nothing can follow must not stall us.
        ascending_after("msg", "msg_ffffffffffffzzzz");
    }
}
