use std::{fs, process::Command, rc::Rc, time::Duration};

use async_std::{future::select, task};
use futures::{channel::{mpsc, oneshot}, prelude::*};
use log::{trace, warn};
use nix::{libc, sys::signal::Signal};
use structopt::StructOpt;
use xidlehook::{
    modules::{StopAt, Xcb},
    timers::CmdTimer,
    Module, Xidlehook,
};

mod signal_handler;
mod socket;

struct Defer<F: FnMut()>(F);
impl<F: FnMut()> Drop for Defer<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}

#[derive(StructOpt, Debug)]
pub struct Opt {
    /// Print the idle time to standard output. This is similar to xprintidle.
    #[structopt(long)]
    pub print: bool,
    /// Exit after the whole chain of timer commands have been invoked
    /// once
    #[structopt(long, conflicts_with("print"))]
    pub once: bool,
    /// Don't invoke the timer when the current application is
    /// fullscreen. Useful for preventing a lockscreen when watching
    /// videos.
    #[structopt(long, conflicts_with("print"))]
    pub not_when_fullscreen: bool,

    /// The duration is the number of seconds of inactivity which
    /// should trigger this timer.
    ///
    /// The command is what is invoked when the idle duration is
    /// reached. It's passed through \"/bin/sh -c\".
    ///
    /// The canceller is what is invoked when the user becomes active
    /// after the timer has gone off, but before the next timer (if
    /// any). Pass an empty string to not have one.
    #[structopt(long, conflicts_with("print"), required_unless("print"), value_names = &["duration", "command", "canceller"])]
    pub timer: Vec<String>,

    /// Don't invoke the timer when any audio is playing (PulseAudio specific)
    #[cfg(feature = "pulse")]
    #[structopt(long, conflicts_with("print"))]
    pub not_when_audio: bool,

    /// Listen to a unix socket at this address for events.
    /// Each event is one line of JSON data.
    #[structopt(long, conflicts_with("print"))]
    pub socket: Option<String>,
}

fn main() -> xidlehook::Result<()> {
    env_logger::init();

    let opt = Opt::from_args();

    let xcb = Rc::new(Xcb::new()?);

    if opt.print {
        let idle = xcb.get_idle()?;
        println!("{}", idle.as_millis());
        return Ok(());
    }

    let mut timers = Vec::new();
    let mut iter = opt.timer.iter().peekable();
    while iter.peek().is_some() {
        // clap-rs will ensure there are always a multiple of 3
        let duration: u64 = match iter.next().unwrap().parse() {
            Ok(duration) => duration,
            Err(err) => {
                eprintln!("error: failed to parse duration as number: {}", err);
                return Ok(());
            },
        };
        timers.push(CmdTimer {
            time: Duration::from_secs(duration),
            activation: Some(command(iter.next().unwrap())),
            abortion: iter.next().filter(|s| !s.is_empty()).map(|s| command(&s)),
            ..CmdTimer::default()
        });
    }

    let mut modules: Vec<Box<dyn Module>> = Vec::new();

    if opt.once {
        modules.push(Box::new(StopAt::completion()));
    }
    if opt.not_when_fullscreen {
        modules.push(Box::new(Rc::clone(&xcb).not_when_fullscreen()));
    }
    #[cfg(feature = "pulse")]
    {
        if opt.not_when_audio {
            modules.push(Box::new(xidlehook::modules::NotWhenAudio::new()?))
        }
    }

    let mut xidlehook = Xidlehook::new(timers).register(modules);

    let (socket_tx, mut socket_rx) = mpsc::channel(4);
    let _scope = if let Some(address) = opt.socket {
        {
            let address = address.clone();
            task::spawn(async move {
                if let Err(err) = socket::socket_loop(&address, socket_tx).await {
                    warn!("Socket handling errored: {}", err);
                }
            });
        }
        Some(Defer(move || {
            trace!("Removing unix socket {}", address);
            let _ = fs::remove_file(&address);
        }))
    } else {
        None
    };

    let (signal_tx, mut signal_rx) = mpsc::channel(1);
    let signal_thread = signal_handler::handle_signals(signal_tx)?;

    loop {
        enum Selected {
            Socket(Option<(socket::Message, oneshot::Sender<socket::Reply>)>),
            Signal(Option<Signal>),
            Exit(xidlehook::Result<()>),
        }

        let a = socket_rx.next().map(Selected::Socket);
        let b = signal_rx.next().map(Selected::Signal);
        let c = xidlehook.main_async(&xcb).map(Selected::Exit);
        let res = task::block_on(select!(a, b, c));

        match res {
            Selected::Socket(data) => {
                if let Some((msg, reply)) = data {
                    trace!("Got command over socket: {:#?}", msg);
                    reply.send(socket::Reply::Empty).unwrap();
                } else {
                    // TODO: Don't poll socket_rx again after this
                }
            },
            Selected::Signal(sig) => {
                if let Some(sig) = sig {
                    trace!("Signal received: {}", sig);
                    break;
                } else {
                    // TODO: Don't poll signal_rx again after this
                }
            },
            Selected::Exit(res) => {
                res?;
            },
        }
    }

    // Call signal handler to pretend there's a signal - which will
    // cause thread to exit
    signal_handler::handler(Signal::SIGINT as i32 as libc::c_int);

    signal_thread.join().unwrap()?;

    Ok(())
}
fn command(cmd: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(cmd);
    command
}
