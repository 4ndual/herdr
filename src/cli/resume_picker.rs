//! `herdr resume-picker` — open the resume-search picker as an in-app overlay.
//!
//! Invoked by the button-dash "Resume search" action script, which resolves the
//! absolute wrapper path from `$HERDR_PLUGIN_ROOT` and passes it here. The host
//! spawns the picker's hidden PTY and shows the floating overlay.

use crate::api::schema::{Method, Request, ResumePickerOpenParams};

pub(super) fn run_resume_picker_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(String::as_str) else {
        print_help();
        return Ok(2);
    };

    match subcommand {
        "open" => open(&args[1..]),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(0)
        }
        other => {
            eprintln!("unknown resume-picker subcommand: {other}");
            print_help();
            Ok(2)
        }
    }
}

fn open(args: &[String]) -> std::io::Result<i32> {
    let mut wrapper: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--wrapper" => {
                let Some(value) = iter.next() else {
                    eprintln!("missing value for --wrapper");
                    return Ok(2);
                };
                wrapper = Some(value.clone());
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    let Some(wrapper) = wrapper else {
        eprintln!("usage: herdr resume-picker open --wrapper <path>");
        return Ok(2);
    };

    super::print_response(&super::send_request(&Request {
        id: "cli:resume-picker:open".into(),
        method: Method::ResumePickerOpen(ResumePickerOpenParams { wrapper }),
    })?)
}

fn print_help() {
    eprintln!("usage: herdr resume-picker open --wrapper <path>");
}
