use super::*;

static ID_SEQUENCE: AtomicU64 = AtomicU64::new(0);
pub type Result<T> = std::result::Result<T, AppError>;

pub fn digest(bytes: impl AsRef<[u8]>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes.as_ref());
    format!("{:x}", hasher.finalize())
}

pub(crate) fn levenshtein(left: &str, right: &str) -> usize {
    let right = right.chars().collect::<Vec<_>>();
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    for (left_index, left_char) in left.chars().enumerate() {
        let mut current = vec![left_index + 1];
        for (right_index, right_char) in right.iter().enumerate() {
            current.push(std::cmp::min(
                std::cmp::min(current[right_index] + 1, previous[right_index + 1] + 1),
                previous[right_index] + usize::from(left_char != *right_char),
            ));
        }
        previous = current;
    }
    previous[right.len()]
}

pub(crate) fn closest_value<'a>(received: &str, allowed_values: &'a [&str]) -> Option<&'a str> {
    let mut ranked = allowed_values
        .iter()
        .map(|value| (*value, levenshtein(received, value)))
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(right.0)));
    let (value, distance) = *ranked.first()?;
    let unique = ranked.get(1).is_none_or(|next| next.1 > distance);
    let width = received.chars().count().max(value.chars().count());
    (unique && distance <= width.div_ceil(3).max(1)).then_some(value)
}

pub fn fresh_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    let sequence = ID_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let stack = &now as *const u128 as usize;
    digest(format!("{now}:{pid}:{sequence}:{stack}").as_bytes())[..32].to_string()
}

pub fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn iso_now() -> String {
    iso_utc(now_epoch())
}

#[cfg(test)]
pub fn iso_utc_millis(epoch_secs: i64, millis: u32) -> String {
    let base = iso_utc(epoch_secs);
    format!("{}.{millis:03}Z", &base[..base.len() - 1])
}

pub fn iso_utc(epoch_secs: i64) -> String {
    let days = epoch_secs.div_euclid(86_400);
    let secs = epoch_secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = secs / 3_600;
    let minute = (secs % 3_600) / 60;
    let second = secs % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(z0: i64) -> (i64, u32, u32) {
    let z = z0 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    y += i64::from(m <= 2);
    (y, m as u32, d as u32)
}
