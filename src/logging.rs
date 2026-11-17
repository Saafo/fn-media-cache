//! Size-bounded local logs. Request payloads and credentials never enter this writer.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

static LOG: OnceLock<Mutex<RotatingLog>> = OnceLock::new();

struct RotatingLog {
    directory: PathBuf,
    file: File,
    size: u64,
    maximum: u64,
    keep: usize,
}
impl RotatingLog {
    fn open(directory: &Path, maximum: u64, keep: usize) -> io::Result<Self> {
        fs::create_dir_all(directory)?;
        let path = directory.join("service.log");
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let _ = crate::config::private_file(&path);
        let size = file.metadata()?.len();
        Ok(Self {
            directory: directory.into(),
            file,
            size,
            maximum,
            keep,
        })
    }
    fn append(&mut self, line: &[u8]) -> io::Result<()> {
        let line = &line[..line.len().min(self.maximum as usize)];
        if self.size.saturating_add(line.len() as u64) > self.maximum {
            self.file.flush()?;
            for n in (1..=self.keep).rev() {
                let old = if n == 1 {
                    self.directory.join("service.log")
                } else {
                    self.directory.join(format!("service.log.{}", n - 1))
                };
                let next = self.directory.join(format!("service.log.{n}"));
                if next.exists() {
                    fs::remove_file(&next)?;
                }
                if old.exists() {
                    fs::rename(old, next)?;
                }
            }
            self.file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.directory.join("service.log"))?;
            let _ = crate::config::private_file(&self.directory.join("service.log"));
            self.size = 0;
        }
        self.file.write_all(line)?;
        self.file.flush()?;
        self.size += line.len() as u64;
        Ok(())
    }
}

pub fn initialize(directory: &Path, maximum: u64, keep: usize) -> io::Result<()> {
    let log = RotatingLog::open(directory, maximum, keep)?;
    let _ = LOG.set(Mutex::new(log));
    Ok(())
}

pub fn write(arguments: std::fmt::Arguments<'_>) {
    let message = arguments.to_string().chars().take(4096).collect::<String>();
    let line = format!(
        "{} {}\n",
        crate::scheduler::now(),
        message.replace(['\n', '\r'], " ")
    );
    if let Some(log) = LOG.get() {
        if let Ok(mut log) = log.lock() {
            let _ = log.append(line.as_bytes());
        }
    } else {
        eprintln!("{line}");
    }
}

#[macro_export]
macro_rules! event { ($($arg:tt)*) => { $crate::logging::write(format_args!($($arg)*)) }; }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rotation_bounds_disk_usage() {
        let dir = std::env::temp_dir().join(format!("fn-log-{}", uuid::Uuid::new_v4()));
        let mut log = RotatingLog::open(&dir, 100, 2).unwrap();
        for _ in 0..20 {
            log.append(&[b'x'; 60]).unwrap();
        }
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 3);
        assert!(
            fs::read_dir(&dir)
                .unwrap()
                .all(|f| f.unwrap().metadata().unwrap().len() <= 100)
        );
        drop(log);
        fs::remove_dir_all(dir).unwrap();
    }
}
