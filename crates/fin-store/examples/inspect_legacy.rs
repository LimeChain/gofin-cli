//! Engineering migration audit, not a second installed public CLI.
fn main() -> std::process::ExitCode {
    let mut args = std::env::args_os().skip(1);
    let Some(path) = args.next().filter(|_| args.len() == 0) else {
        eprintln!("Supply exactly one historical journal path; no input records are printed.");
        return std::process::ExitCode::FAILURE;
    };
    match fin_store::legacy::inspect(std::path::PathBuf::from(path)) {
        Ok(report) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&report).expect("safe report")
            );
            if report.safe_to_start_separate_rust_run {
                std::process::ExitCode::SUCCESS
            } else {
                std::process::ExitCode::from(2)
            }
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}
