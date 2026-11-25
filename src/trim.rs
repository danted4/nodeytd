//! Flexible trim times, as the Node CLI accepted them: `3` or `03` (seconds),
//! `3:00` (minutes), `1:02:03` (hours). Blank means "not set".

use anyhow::{bail, Result};

/// Seconds for a trim time, or `None` when the field is blank.
pub fn parse_flexible_time(raw: &str) -> Result<Option<f64>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let parts: Vec<&str> = raw.split(':').collect();
    if parts.len() > 3 {
        bail!("use seconds, m:ss or h:mm:ss");
    }
    let mut seconds = 0.0;
    for (index, part) in parts.iter().enumerate() {
        let part = part.trim();
        let value: f64 = part
            .parse()
            .ok()
            .filter(|value: &f64| value.is_finite() && *value >= 0.0)
            .ok_or_else(|| anyhow::anyhow!("'{raw}' is not a time"))?;
        let is_last = index == parts.len() - 1;
        if !is_last && part.contains('.') {
            bail!("only seconds may have a fraction");
        }
        seconds = seconds * 60.0 + value;
    }
    Ok(Some(seconds))
}

/// A validated trim range. Either end may be open.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TrimRange {
    pub start: Option<f64>,
    pub end: Option<f64>,
}

impl TrimRange {
    pub fn parse(start: &str, end: &str) -> Result<Self> {
        let start = parse_flexible_time(start)?;
        let end = parse_flexible_time(end)?;
        if let (Some(s), Some(e)) = (start, end) {
            if e <= s {
                bail!("trim end must be after trim start");
            }
        }
        Ok(Self { start, end })
    }

    pub fn is_set(&self) -> bool {
        self.start.is_some() || self.end.is_some()
    }

    /// The `--download-sections` value yt-dlp takes, e.g. `*3-10` or `*60-inf`.
    pub fn sections_arg(&self) -> Option<String> {
        if !self.is_set() {
            return None;
        }
        let start = self.start.unwrap_or(0.0);
        let end = self
            .end
            .map(format_seconds)
            .unwrap_or_else(|| "inf".to_string());
        Some(format!("*{}-{}", format_seconds(start), end))
    }
}

fn format_seconds(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{}", value as u64)
    } else {
        format!("{value:.3}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_node_cli_formats() {
        assert_eq!(parse_flexible_time("").unwrap(), None);
        assert_eq!(parse_flexible_time("3").unwrap(), Some(3.0));
        assert_eq!(parse_flexible_time("03").unwrap(), Some(3.0));
        assert_eq!(parse_flexible_time("3:00").unwrap(), Some(180.0));
        assert_eq!(parse_flexible_time("1:02:03").unwrap(), Some(3723.0));
        assert_eq!(parse_flexible_time("0:10.5").unwrap(), Some(10.5));
    }

    #[test]
    fn rejects_junk() {
        assert!(parse_flexible_time("abc").is_err());
        assert!(parse_flexible_time("1:2:3:4").is_err());
        assert!(parse_flexible_time("-3").is_err());
        assert!(parse_flexible_time("1.5:00").is_err());
    }

    #[test]
    fn range_validation_and_arg() {
        assert!(TrimRange::parse("10", "5").is_err());
        let range = TrimRange::parse("3", "0:10").unwrap();
        assert_eq!(range.sections_arg().as_deref(), Some("*3-10"));
        let open_end = TrimRange::parse("1:00", "").unwrap();
        assert_eq!(open_end.sections_arg().as_deref(), Some("*60-inf"));
        let open_start = TrimRange::parse("", "5").unwrap();
        assert_eq!(open_start.sections_arg().as_deref(), Some("*0-5"));
        assert_eq!(TrimRange::parse("", "").unwrap().sections_arg(), None);
    }
}
