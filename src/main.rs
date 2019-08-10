#![feature(async_await)]

extern crate clobber;

use std::io::{stdin, Read};
use std::net::SocketAddr;
use std::thread;
use std::time::Duration;

use clap::{App, Arg, ArgMatches};
use humantime;
use log::LevelFilter;

use clobber::{tcp, Config, Message};

fn main() {
    let cli = cli();
    let matches = cli.get_matches();
    let settings = settings_from_argmatches(&matches);

    let log_level = match &matches.occurrences_of("v") {
        1 => log::LevelFilter::Info,
        2 => log::LevelFilter::Debug,
        3 => log::LevelFilter::Trace,
        _ => log::LevelFilter::Warn,
    };

    setup_logger(log_level).expect("Failed to setup logger");

    let bytes = match optional_stdin() {
        Some(bytes) => bytes,
        None => unimplemented!("no request body"), // todo: Load from file
    };

    tcp::clobber(settings, Message::new(bytes)).expect("Failed to clobber :(");
}

fn cli() -> App<'static, 'static> {
    App::new("clobber")
        .version("0.1")
        .about("tcp load testing tool")
        .arg(
            Arg::with_name("target")
                .short("t")
                .long("target")
                .help("Host to clobber")
                .takes_value(true)
                .required(true),
        )
        .arg(
            Arg::with_name("rate")
                .long("rate")
                .help("Limit to a particular rate per-second.")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("threads")
                .long("threads")
                .help("Number of threads")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("v")
                .short("v")
                .multiple(true)
                .help("Sets the log level, from -v to -vvv"),
        )
        .arg(
            Arg::with_name("connections")
                .short("c")
                .long("connections")
                .help("Max number of open connections at any given time")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("duration")
                .short("d")
                .long("duration")
                .help("Length of the run (e.g. 5s, 10m, 2h, etc...)")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("connect-timeout")
                .long("connect-timeout")
                .help("Timeout for initial TCP syn timeout")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("read-timeout")
                .long("read-timeout")
                .help("Timeout for reading response from target")
                .takes_value(true),
        )
}

fn settings_from_argmatches(matches: &ArgMatches) -> Config {
    let target = matches
        .value_of("target")
        .unwrap()
        .parse::<SocketAddr>()
        .expect("Failed to parse target");

    let rate = matches
        .value_of("rate")
        .unwrap_or("0")
        .parse::<u32>()
        .expect("Failed to parse rate");

    let connections = matches
        .value_of("connections")
        .unwrap_or("100")
        .parse::<u32>()
        .expect("Failed to parse connections");

    let connect_timeout = match matches.value_of("connect-timeout") {
        Some(timeout) => Some(timeout.parse().expect("Failed to parse connect_timeout")),
        None => None,
    };

    let read_timeout = match matches.value_of("read-timeout") {
        Some(timeout) => Some(timeout.parse().expect("Failed to parse read_timeout")),
        None => None,
    };

    let num_threads = match matches.value_of("threads") {
        Some(num_threads) => Some(num_threads.parse::<u32>().expect("Failed to parse threads")),
        None => None,
    };

    let duration = match matches.value_of("duration") {
        Some(s) => Some(humantime::parse_duration(s).expect("Failed to parse duration")),
        None => None,
    };

    let rate = match rate {
        0 => None,
        n => Some(n),
    };

    //    if num_threads == 0 {
    //        num_threads = num_cpus::get() as u32;
    //    }

    Config {
        rate,
        target,
        duration,
        connections,
        num_threads,
        read_timeout,    // todo make this optional
        connect_timeout, // todo make this optional
    }
}

pub fn setup_logger(log_level: LevelFilter) -> Result<(), Box<dyn std::error::Error>> {
    fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "{}[{}][{}] {}",
                chrono::Local::now().format("[%Y-%m-%d][%H:%M:%S]"),
                record.target(),
                record.level(),
                message
            ))
        })
        .level(log_level)
        .chain(std::io::stdout())
        .chain(fern::log_file("clobber.log")?)
        .apply()?;

    Ok(())
}

/// This is a bit of a weird way to do this, but I'm not sure what the better option is.
/// What this bit of code does is that it spins off a thread to listen to stdin, and then
/// sleeps for a moment to give that thread time to put something in the channel. It... works.
/// Surely there is a more idiomatic option though. ‾\_(ツ)_/‾
fn optional_stdin() -> Option<Vec<u8>> {
    let (sender, receiver) = std::sync::mpsc::channel();

    thread::spawn(move || {
        let sin = stdin();
        let mut bytes = vec![];

        sin.lock()
            .read_to_end(&mut bytes)
            .expect("Failed to read from stdin");

        sender.send(bytes).expect("Failed to send input");
    });

    thread::sleep(Duration::from_millis(1));

    match receiver.try_recv() {
        Ok(l) => Some(l),
        _ => None,
    }
}
