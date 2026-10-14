//! CSV journal flushed every sample + TCX rewritten atomically every minute.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Duration, SecondsFormat, Utc};

use crate::session::Sample;

const HEADER: &str = "clock,power,target,hr,cadence,speed_kmh,distance_m";
const TCX_EVERY_SECS: u64 = 60;
const FSYNC_EVERY_SECS: u64 = 30;

pub struct Recorder {
    start: DateTime<Utc>,
    csv: BufWriter<File>,
    csv_path: PathBuf,
    tcx_path: PathBuf,
    samples: Vec<Sample>,
    last_tcx: Instant,
    last_sync: Instant,
}

impl Recorder {
    pub fn create(dir: &Path, workout_name: &str) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let start = Utc::now();
        let slug: String = workout_name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
            .collect();
        let base = format!("{}_{}", chrono::Local::now().format("%Y-%m-%d_%H%M%S"), slug.trim_matches('-'));
        let csv_path = dir.join(format!("{base}.csv"));
        let tcx_path = dir.join(format!("{base}.tcx"));
        let mut csv = BufWriter::new(File::create(&csv_path).with_context(|| format!("cannot create {}", csv_path.display()))?);
        writeln!(csv, "# watty start={}", start.to_rfc3339_opts(SecondsFormat::Secs, true))?;
        writeln!(csv, "{HEADER}")?;
        csv.flush()?;
        Ok(Self { start, csv, csv_path, tcx_path, samples: Vec::new(), last_tcx: Instant::now(), last_sync: Instant::now() })
    }

    pub fn csv_path(&self) -> &Path {
        &self.csv_path
    }

    pub fn tcx_path(&self) -> &Path {
        &self.tcx_path
    }

    pub fn record(&mut self, s: Sample) -> Result<()> {
        writeln!(self.csv, "{}", csv_line(&s))?;
        self.csv.flush()?;
        if self.last_sync.elapsed().as_secs() >= FSYNC_EVERY_SECS {
            self.csv.get_ref().sync_data()?;
            self.last_sync = Instant::now();
        }
        self.samples.push(s);
        if self.last_tcx.elapsed().as_secs() >= TCX_EVERY_SECS {
            self.write_tcx()?;
        }
        Ok(())
    }

    pub fn write_tcx(&mut self) -> Result<()> {
        self.last_tcx = Instant::now();
        write_atomic(&self.tcx_path, &tcx(self.start, &self.samples))
    }

    /// Deletes everything recorded so far.
    pub fn discard(self) -> Result<()> {
        let Self { csv, csv_path, tcx_path, .. } = self;
        drop(csv);
        std::fs::remove_file(&csv_path).with_context(|| format!("cannot remove {}", csv_path.display()))?;
        for path in [tcx_path.clone(), tcx_path.with_extension("tcx.tmp")] {
            match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(e).with_context(|| format!("cannot remove {}", path.display()));
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<usize> {
        self.csv.flush()?;
        self.csv.get_ref().sync_all()?;
        self.write_tcx()?;
        Ok(self.samples.len())
    }
}

fn opt<T: ToString>(v: Option<T>) -> String {
    v.map(|v| v.to_string()).unwrap_or_default()
}

fn csv_line(s: &Sample) -> String {
    format!(
        "{},{},{},{},{},{},{:.1}",
        s.clock,
        opt(s.power),
        opt(s.target),
        opt(s.hr),
        opt(s.cadence),
        opt(s.speed_kmh.map(|v| format!("{v:.2}"))),
        s.distance_m
    )
}

fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("tcx.tmp");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("cannot write {}", path.display()))
}

pub fn read_journal(path: &Path) -> Result<(DateTime<Utc>, Vec<Sample>)> {
    let f = BufReader::new(File::open(path).with_context(|| format!("cannot open {}", path.display()))?);
    let mut start = None;
    let mut samples = Vec::new();
    for (n, line) in f.lines().enumerate() {
        let line = line?;
        if let Some(rest) = line.strip_prefix("# watty start=") {
            start = Some(DateTime::parse_from_rfc3339(rest.trim())?.with_timezone(&Utc));
            continue;
        }
        if line.is_empty() || line == HEADER {
            continue;
        }
        let f: Vec<&str> = line.split(',').collect();
        // A crash can leave a half-written last line.
        if f.len() != 7 {
            tracing::warn!("{}:{}: skipping malformed line", path.display(), n + 1);
            continue;
        }
        let num = |i: usize| f[i].parse::<u16>().ok();
        let Ok(clock) = f[0].parse() else { continue };
        samples.push(Sample {
            clock,
            power: num(1),
            target: num(2),
            hr: num(3),
            cadence: num(4),
            speed_kmh: f[5].parse().ok(),
            distance_m: f[6].parse().unwrap_or(0.0),
        });
    }
    Ok((start.ok_or_else(|| anyhow!("missing start header"))?, samples))
}

pub fn export(csv: &Path, out: Option<PathBuf>) -> Result<(PathBuf, usize)> {
    let (start, samples) = read_journal(csv)?;
    let out = out.unwrap_or_else(|| csv.with_extension("tcx"));
    write_atomic(&out, &tcx(start, &samples))?;
    Ok((out, samples.len()))
}

fn iso(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub fn tcx(start: DateTime<Utc>, samples: &[Sample]) -> String {
    let total = samples.len();
    let distance = samples.last().map_or(0.0, |s| s.distance_m);
    let kj: f64 = samples.iter().filter_map(|s| s.power).map(|p| p as f64 / 1000.0).sum();
    let hrs: Vec<u16> = samples.iter().filter_map(|s| s.hr).collect();
    let max_speed = samples.iter().filter_map(|s| s.speed_kmh).fold(0.0, f64::max) / 3.6;

    let mut x = String::with_capacity(400 + samples.len() * 300);
    x.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>
<TrainingCenterDatabase xmlns="http://www.garmin.com/xmlschemas/TrainingCenterDatabase/v2" xmlns:ns3="http://www.garmin.com/xmlschemas/ActivityExtension/v2">
 <Activities>
  <Activity Sport="Biking">
"#);
    let _ = writeln!(x, "   <Id>{}</Id>", iso(start));
    let _ = writeln!(x, "   <Lap StartTime=\"{}\">", iso(start));
    let _ = writeln!(x, "    <TotalTimeSeconds>{total}</TotalTimeSeconds>");
    let _ = writeln!(x, "    <DistanceMeters>{distance:.1}</DistanceMeters>");
    let _ = writeln!(x, "    <MaximumSpeed>{max_speed:.2}</MaximumSpeed>");
    // kJ of work ≈ kcal burned for cycling (~24% efficiency).
    let _ = writeln!(x, "    <Calories>{}</Calories>", kj.round() as u32);
    if !hrs.is_empty() {
        let avg = hrs.iter().map(|&h| h as u32).sum::<u32>() / hrs.len() as u32;
        let _ = writeln!(x, "    <AverageHeartRateBpm><Value>{avg}</Value></AverageHeartRateBpm>");
        let _ = writeln!(x, "    <MaximumHeartRateBpm><Value>{}</Value></MaximumHeartRateBpm>", hrs.iter().max().unwrap());
    }
    x.push_str("    <Intensity>Active</Intensity>\n    <TriggerMethod>Manual</TriggerMethod>\n    <Track>\n");
    for s in samples {
        let _ = writeln!(x, "     <Trackpoint>");
        let _ = writeln!(x, "      <Time>{}</Time>", iso(start + Duration::seconds(s.clock as i64)));
        let _ = writeln!(x, "      <DistanceMeters>{:.1}</DistanceMeters>", s.distance_m);
        if let Some(hr) = s.hr.filter(|&h| h > 0) {
            let _ = writeln!(x, "      <HeartRateBpm><Value>{hr}</Value></HeartRateBpm>");
        }
        if let Some(c) = s.cadence {
            let _ = writeln!(x, "      <Cadence>{}</Cadence>", c.min(254));
        }
        if s.power.is_some() || s.speed_kmh.is_some() {
            x.push_str("      <Extensions><ns3:TPX>");
            if let Some(v) = s.speed_kmh {
                let _ = write!(x, "<ns3:Speed>{:.2}</ns3:Speed>", v / 3.6);
            }
            if let Some(p) = s.power {
                let _ = write!(x, "<ns3:Watts>{p}</ns3:Watts>");
            }
            x.push_str("</ns3:TPX></Extensions>\n");
        }
        x.push_str("     </Trackpoint>\n");
    }
    x.push_str("    </Track>\n   </Lap>\n  </Activity>\n </Activities>\n</TrainingCenterDatabase>\n");
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(clock: u32, power: Option<u16>, hr: Option<u16>) -> Sample {
        Sample { clock, power, target: Some(200), hr, cadence: Some(90), speed_kmh: Some(30.0), distance_m: clock as f64 * 8.3 }
    }

    #[test]
    fn journal_roundtrip_and_export() {
        let dir = std::env::temp_dir().join(format!("watty-test-{}", std::process::id()));
        let mut r = Recorder::create(&dir, "Sweet Spot 3x10").unwrap();
        r.record(sample(1, Some(200), Some(120))).unwrap();
        r.record(sample(2, None, None)).unwrap();
        let csv = r.csv_path().to_path_buf();
        let tcx_path = r.tcx_path().to_path_buf();
        assert!(csv.file_name().unwrap().to_str().unwrap().ends_with("sweet-spot-3x10.csv"));
        assert_eq!(r.finish().unwrap(), 2);

        // Simulate a crash mid-write.
        std::fs::OpenOptions::new().append(true).open(&csv).unwrap().write_all(b"3,21").unwrap();
        let (_, samples) = read_journal(&csv).unwrap();
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].power, Some(200));
        assert_eq!(samples[1].power, None);
        assert_eq!(samples[1].hr, None);

        let (out, n) = export(&csv, None).unwrap();
        assert_eq!((out, n), (tcx_path.clone(), 2));
        let xml = std::fs::read_to_string(&tcx_path).unwrap();
        roxmltree::Document::parse(&xml).unwrap();
        assert!(xml.contains("<ns3:Watts>200</ns3:Watts>"));
        assert!(xml.contains("<HeartRateBpm><Value>120</Value></HeartRateBpm>"));
        assert_eq!(xml.matches("<Trackpoint>").count(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn discard_removes_files() {
        let dir = std::env::temp_dir().join(format!("watty-discard-{}", std::process::id()));
        let mut r = Recorder::create(&dir, "Test").unwrap();
        r.record(sample(1, Some(200), None)).unwrap();
        r.write_tcx().unwrap();
        r.discard().unwrap();
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tcx_timestamps() {
        let start = DateTime::parse_from_rfc3339("2026-10-11T10:00:00Z").unwrap().with_timezone(&Utc);
        let xml = tcx(start, &[sample(1, Some(100), None), sample(61, Some(100), None)]);
        assert!(xml.contains("<Time>2026-10-11T10:00:01Z</Time>"));
        assert!(xml.contains("<Time>2026-10-11T10:01:01Z</Time>"));
        assert!(xml.contains("<TotalTimeSeconds>2</TotalTimeSeconds>"));
        assert!(!xml.contains("AverageHeartRateBpm"));
    }
}
