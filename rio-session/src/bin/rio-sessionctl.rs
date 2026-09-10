use rio_session::{SessionClient, SessionDescriptor};
use std::env;
use std::path::PathBuf;

fn usage() -> ! {
    eprintln!("usage: rio-sessionctl snapshot <descriptor> | write <descriptor> <bytes>");
    std::process::exit(2);
}

fn main() {
    let mut args = env::args_os().skip(1);
    let command = args.next().unwrap_or_else(|| usage());
    let descriptor = args.next().unwrap_or_else(|| usage());
    let descriptor =
        SessionDescriptor::load(&PathBuf::from(descriptor)).unwrap_or_else(|error| {
            eprintln!("load descriptor: {error}");
            std::process::exit(1);
        });
    let client = SessionClient::attach(descriptor).unwrap_or_else(|error| {
        eprintln!("attach session: {error}");
        std::process::exit(1);
    });

    match command.to_str() {
        Some("snapshot") => {
            let frame = client.snapshot().unwrap_or_else(|error| {
                eprintln!("snapshot: {error}");
                std::process::exit(1);
            });
            println!(
                "{}x{} sequence={} title={:?} cwd={:?}",
                frame.columns,
                frame.lines,
                frame.sequence,
                frame.title,
                frame.working_dir
            );
            for row in frame.rows {
                println!("{}", row.text);
            }
        }
        Some("write") => {
            let bytes = args
                .next()
                .unwrap_or_else(|| usage())
                .to_string_lossy()
                .into_owned();
            client.write(bytes.into_bytes()).unwrap_or_else(|error| {
                eprintln!("write: {error}");
                std::process::exit(1);
            });
        }
        _ => usage(),
    }
}
