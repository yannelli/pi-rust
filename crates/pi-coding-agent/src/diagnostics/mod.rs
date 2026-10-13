//! Diagnostics: `/bug` reports with redaction, zip export, and Radius upload; crash logs;
//! startup timings (`PI_TIMING`); and the stdout guard for JSON and RPC modes. Resource
//! diagnostics (upstream `core/diagnostics.ts`) live in [`crate::resources`].

pub mod bug_report;
pub mod bug_summary;
pub mod bug_upload;
pub mod crash_log;
pub mod output_guard;
pub mod redact;
pub mod stack;
pub mod timings;
pub mod zip;

#[cfg(test)]
mod tests;

pub use crate::resources::{ResourceCollision, ResourceDiagnostic};
pub use bug_report::{
    BUG_REPORT_CUSTOM_ENTRY_TYPE, BugReportBundle, BugReportFile, BugReportMetadataOptions, BugReportSessionEntryData,
    ExtensionDescription, ProviderDescription, bug_report_archive_file_name, bug_report_files,
    collect_bug_report_diagnostics, collect_bug_report_metadata, write_bug_report_archive,
};
pub use bug_summary::{BugReportSummaryOptions, generate_bug_report_summary};
pub use bug_upload::{UploadBugReportOptions, radius_gateway_url, upload_bug_report};
pub use crash_log::{
    Crash, CrashContext, CrashRecord, clear_crash_log, crash_log_path, crash_report_instructions, install_crash_hook,
    read_crash_log, record_crash, record_fatal_error, take_unnotified_crash,
};
pub use output_guard::{
    flush_raw_stdout, guarded_stdout, is_stdout_taken_over, restore_stdout, say_stdout, take_over_stdout,
    write_raw_stdout,
};
pub use redact::{redact_json_value, redact_url};
pub use stack::{ExtensionStackInfo, find_extension_stack_matches, format_crash_extension_hint};
pub use timings::{print_timings, reset_timings, time, time_in};
