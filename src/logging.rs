use std::io::{self, Write};
use std::sync::LazyLock;

use chrono::Local;
use log::{Level, LevelFilter, Log};
#[cfg(all(unix, feature = "journald"))]
use systemd_journal_logger::{JournalLog, connected_to_journal, current_exe_identifier};

/// A simple logger that writes messages to `stderr`.
///
/// Colour support is automatically provided by the [`anstream`] crate.
pub struct Logger {
    filter: LevelFilter,
    timestamps: bool,
    #[cfg(all(unix, feature = "journald"))]
    journald: Option<JournalLog>,
}

/// Timestamp format for log output. Format is `Jul 8 2001 14:46:23`.
static TIMESTAMP_FMT: LazyLock<&'static [chrono::format::Item<'static>]> = LazyLock::new(|| {
    // NB: `LazyLock`'s own docs have a note about how static items don't ever get dropped, so leaking this Vec into a
    // static slice doesn't make any difference in that regard.
    chrono::format::StrftimeItems::new("%b %d %Y %H:%M:%S")
        .parse_to_owned()
        .expect("hardcoded strftime string should be valid")
        .leak()
});

/// Predefined styles for log levels.
///
/// Level styles are taken from systemd's implementation so as to match `journalctl` output. Mapping from
/// [`log::Level`] into the right style is done to match the way [`JournalLog` does it][JournalLog-mapping]:
///
/// - `Level::Error` → `3` (err)
/// - `Level::Warn` → `4` (warning)
/// - `Level::Info` → `5` (notice)
/// - `Level::Debug` → `6` (info)
/// - `Level::Trace` → `7` (debug)
///
/// The specific colours borrowed from systemd are from (as of commit `19deb47`):
///
/// - Systemd maps log levels to ANSI colours in [`/src/basic/terminal-util.c`].
/// - Systemd's ANSI definitions are specified in [`/src/basic/ansi-color.h`].
///
/// [`/src/basic/terminal-util.c`]: https://github.com/systemd/systemd/blob/19deb47ade9d54160e7ba5f8f2a995e3f22df678/src/basic/terminal-util.c#L1811-L1847
/// [`/src/basic/ansi-color.h`]: https://github.com/systemd/systemd/blob/19deb47ade9d54160e7ba5f8f2a995e3f22df678/src/basic/ansi-color.h
/// [JournalLog-mapping]: https://docs.rs/systemd-journal-logger/2.2.2/systemd_journal_logger/struct.JournalLog.html#log-levels-and-priorities
#[rustfmt::skip]
mod styles {
    use anstyle::{Ansi256Color, AnsiColor, Color, Style};

    pub const TRACE: Style = Style::new().fg_color(Some(ansi256(245)));                 // (ansi-color.h:53) `ANSI_GREY`
    pub const DEBUG: Style = Style::new().fg_color(None);                               // Excluded from
    pub const INFO: Style  = Style::new().fg_color(None).bold();                        // (ansi-color.h:84) `ANSI_HIGHLIGHT`
    pub const WARN: Style  = Style::new().fg_color(Some(ansi256(185))).bold();          // (ansi-color.h:65) `ANSI_HIGHLIGHT_KHAKI3`
    pub const ERROR: Style = Style::new().fg_color(Some(ansi(AnsiColor::Red))).bold();  // (ansi-color.h:57) `ANSI_HIGHLIGHT_RED`

    /// So I don't have to type such long struct/enum names for colours.
    const fn ansi256(color: u8) -> Color {
        Color::Ansi256(Ansi256Color(color))
    }

    const fn ansi(color: AnsiColor) -> Color {
        Color::Ansi(color)
    }
}

impl Logger {
    /// Creates a new logger instance.
    pub fn new(level: LevelFilter) -> Self {
        // Default for timestamps is enabled, but they an be disabled by setting an environment variable.
        let mut timestamps = true;
        if crate::get_var("PORKBUN_LOG_NO_TIMESTAMPS").is_ok_and(|v| !v.is_empty()) {
            timestamps = false;
        }

        // If `journald` connects successfully, we don't need to print our own timestamps.
        #[cfg(all(unix, feature = "journald"))]
        let journald = init_journald().inspect(|_| timestamps = false);

        Self {
            filter: level,
            timestamps,
            #[cfg(all(unix, feature = "journald"))]
            journald,
        }
    }

    /// Initializes this logger.
    pub fn init(self) -> Result<(), log::SetLoggerError> {
        let level = self.filter;
        log::set_boxed_logger(Box::new(self)).map(|_| log::set_max_level(level))
    }

    /// Fallible version of [`Log::log`] to enable the use of `?` within.
    fn try_log(&self, record: &log::Record) -> io::Result<()> {
        // Only log our own messages; hide implementation details (reqwest also has logging)
        if !record.target().starts_with(env!("CARGO_CRATE_NAME")) {
            return Ok(());
        }

        if !self.enabled(record.metadata()) {
            return Ok(());
        }

        // If we have a journald connection, forward the message directly there instead of printing it ourselves.
        // [TODO] Decide if I want to do my own level filtering first, or if I just want to forward everything on to
        //        journald and then do filtering there. Filtering myself first will help keep logs small for a service
        //        that runs so often (every 15-30 minutes).
        #[cfg(all(unix, feature = "journald"))]
        if let Some(journald) = self.journald.as_ref() {
            return journald.journal_send(record); // Also returns io::Result
        }

        // `anstream`'s versions of `stderr` will automatically handle terminal/VT configuration and NO_COLOR support.
        let mut output = anstream::stderr().lock();

        #[rustfmt::skip]
        let (style, tag) = match record.level() {
            Level::Trace => (styles::TRACE, "[trace]"),
            Level::Debug => (styles::DEBUG, "[debug]"),
            Level::Info  => ( styles::INFO, "[info]"),
            Level::Warn  => ( styles::WARN, "[warn]"),
            Level::Error => (styles::ERROR, "[error]"),
        };

        if self.timestamps {
            let timestamp = Local::now().format_with_items(TIMESTAMP_FMT.iter());
            write!(output, "{timestamp} ")?;
        }

        if !record.target().is_empty() {
            write!(output, "{} ", record.target())?;
        }

        writeln!(output, "{style}{tag} {}{style:#}", record.args())?;
        output.flush()?;
        Ok(())
    }
}

impl Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= self.filter
    }

    fn log(&self, record: &log::Record) {
        let _ = self.try_log(record);
    }

    fn flush(&self) {
        let _ = anstream::stderr().flush();

        #[cfg(all(unix, feature = "journald"))]
        if let Some(journald) = self.journald.as_ref() {
            <JournalLog as Log>::flush(journald);
        }
    }
}

#[cfg(all(unix, feature = "journald"))]
fn init_journald() -> Option<JournalLog> {
    if connected_to_journal() {
        let identifier = current_exe_identifier().unwrap_or_else(|| env!("CARGO_PKG_NAME").to_string());
        let logger = JournalLog::empty()
            .ok()?
            .with_syslog_identifier(identifier)
            .add_extra_field("version", env!("CARGO_PKG_VERSION"));
        Some(logger)
    } else {
        None
    }
}
