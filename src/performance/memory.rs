//! Resident memory sampled while an operation runs, not only at its ends:
//! a background thread reads it every interval until stopped and keeps the
//! peak. Linux reads `/proc/self/status` (VmRSS, and VmHWM for the process's
//! own peak); Windows runs one hidden PowerShell process that prints the
//! working set and its peak in a loop. Other platforms report it as
//! unavailable.
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

/// What the samples showed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Peak {
    pub samples: usize,
    pub first_bytes: Option<u64>,
    pub peak_bytes: Option<u64>,
    pub last_bytes: Option<u64>,
    /// The highest the process ever reached, as the system counts it.
    pub process_peak_bytes: Option<u64>,
    pub error: Option<String>,
}

impl Peak {
    fn add(&mut self, bytes: u64, process_peak: Option<u64>) {
        self.samples += 1;
        self.first_bytes.get_or_insert(bytes);
        self.last_bytes = Some(bytes);
        self.peak_bytes = Some(self.peak_bytes.map_or(bytes, |peak| peak.max(bytes)));
        if process_peak.is_some() {
            self.process_peak_bytes = process_peak;
        }
    }
}

/// A running sampler; [`Sampler::stop`] ends it and gives the peak.
pub struct Sampler {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<Peak>>,
    #[cfg(windows)]
    child: Option<std::process::Child>,
    error: Option<String>,
}

impl Sampler {
    /// Starts sampling every `interval`.
    pub fn start(interval: Duration) -> Self {
        let (stop, stopped) = mpsc::channel::<()>();
        let mut sampler = Self {
            stop: Some(stop),
            thread: None,
            #[cfg(windows)]
            child: None,
            error: None,
        };
        sampler.spawn(interval, stopped);
        sampler
    }

    #[cfg(target_os = "linux")]
    fn spawn(&mut self, interval: Duration, stopped: mpsc::Receiver<()>) {
        let spawned = std::thread::Builder::new()
            .name("performance-memory".into())
            .spawn(move || {
                let mut peak = Peak::default();
                loop {
                    match std::fs::read_to_string("/proc/self/status")
                        .ok()
                        .and_then(|status| linux_sample(&status))
                    {
                        Some((bytes, high)) => peak.add(bytes, high),
                        None => {
                            peak.error = Some("Could not read /proc/self/status".into());
                            return peak;
                        }
                    }
                    if !matches!(
                        stopped.recv_timeout(interval),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    ) {
                        return peak;
                    }
                }
            });
        match spawned {
            Ok(thread) => self.thread = Some(thread),
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    #[cfg(windows)]
    fn spawn(&mut self, interval: Duration, _stopped: mpsc::Receiver<()>) {
        use std::io::BufRead as _;
        use std::os::windows::process::CommandExt as _;
        use std::process::{Command, Stdio};
        let script = format!(
            "$id={}; while ($true) {{ $p = Get-Process -Id $id -ErrorAction Stop; \
             [Console]::Out.WriteLine(\"$($p.WorkingSet64) $($p.PeakWorkingSet64)\"); \
             [Console]::Out.Flush(); Start-Sleep -Milliseconds {} }}",
            std::process::id(),
            interval.as_millis().max(10)
        );
        let child = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            // CREATE_NO_WINDOW
            .creation_flags(0x0800_0000)
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(error) => {
                self.error = Some(error.to_string());
                return;
            }
        };
        let Some(stdout) = child.stdout.take() else {
            self.error = Some("The memory sampler has no output".into());
            let _ = child.kill();
            let _ = child.wait();
            return;
        };
        let spawned = std::thread::Builder::new()
            .name("performance-memory".into())
            .spawn(move || {
                let mut peak = Peak::default();
                for line in std::io::BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    if let Some((bytes, high)) = windows_sample(&line) {
                        peak.add(bytes, high);
                    }
                }
                peak
            });
        match spawned {
            Ok(thread) => self.thread = Some(thread),
            Err(error) => self.error = Some(error.to_string()),
        }
        self.child = Some(child);
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    fn spawn(&mut self, _: Duration, _: mpsc::Receiver<()>) {
        self.error = Some("Memory sampling is supported on Linux and Windows".into());
    }

    /// Stops sampling and gives what it saw. Blocks until the sampler's
    /// thread ends; call it off the UI thread.
    pub fn stop(mut self) -> Peak {
        // Dropping the sender wakes the Linux thread at once.
        self.stop.take();
        #[cfg(windows)]
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let mut peak = match self.thread.take().map(JoinHandle::join) {
            Some(Ok(peak)) => peak,
            Some(Err(_)) => Peak {
                error: Some("The memory sampler stopped unexpectedly".into()),
                ..Peak::default()
            },
            None => Peak::default(),
        };
        if peak.error.is_none() {
            peak.error = self.error.take();
        }
        if peak.samples == 0 && peak.error.is_none() {
            peak.error = Some("No memory sample was taken".into());
        }
        peak
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The resident set and the process's peak, in bytes, from
/// `/proc/self/status`.
#[cfg(any(target_os = "linux", test))]
fn linux_sample(status: &str) -> Option<(u64, Option<u64>)> {
    let field = |name: &str| -> Option<u64> {
        let kib = status
            .lines()
            .find_map(|line| line.strip_prefix(name))?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()?;
        Some(kib * 1024)
    };
    Some((field("VmRSS:")?, field("VmHWM:")))
}

/// The working set and its peak, in bytes, from one line of the sampler.
#[cfg(any(windows, test))]
fn windows_sample(line: &str) -> Option<(u64, Option<u64>)> {
    let mut numbers = line.split_whitespace().map(str::parse::<u64>);
    let bytes = numbers.next()?.ok()?;
    let peak = numbers.next().and_then(Result::ok);
    Some((bytes, peak))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_are_read_from_each_platform_s_text() {
        let status = "Name:\tjykre\nVmHWM:\t  2048 kB\nVmRSS:\t  1024 kB\n";
        assert_eq!(linux_sample(status), Some((1024 * 1024, Some(2048 * 1024))));
        assert_eq!(linux_sample("Name: x\n"), None);
        assert_eq!(
            windows_sample("123456 999999"),
            Some((123_456, Some(999_999)))
        );
        assert_eq!(windows_sample("123456"), Some((123_456, None)));
        assert_eq!(windows_sample("not a number"), None);
    }

    #[test]
    fn the_peak_is_the_largest_sample() {
        let mut peak = Peak::default();
        peak.add(10, None);
        peak.add(30, Some(40));
        peak.add(20, None);
        assert_eq!(peak.samples, 3);
        assert_eq!(
            (peak.first_bytes, peak.peak_bytes, peak.last_bytes),
            (Some(10), Some(30), Some(20))
        );
        assert_eq!(peak.process_peak_bytes, Some(40));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_sampler_reads_this_process() {
        let sampler = Sampler::start(Duration::from_millis(5));
        std::thread::sleep(Duration::from_millis(30));
        let peak = sampler.stop();
        assert!(peak.samples >= 1, "{peak:?}");
        assert!(peak.peak_bytes.unwrap_or(0) > 0);
    }
}
