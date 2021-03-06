//! The SnapFaaS Controller
//!
//! The Controller consists of a request manager (file or HTTP) and a pool of workers.
//! The gateway takes in requests. The controller assigns each request a worker.
//! Each worker is responsible for finding a VM to handle the request and proxies the response.
//!
//! The Controller maintains several states:
//!   1. kernel path
//!   2. kernel boot argument
//!   3. function store and their files' locations

use clap::{App, Arg};
use log::{error, warn, info, trace};
use snapfaas::configs;
use snapfaas::controller::Controller;
use snapfaas::gateway;
use snapfaas::gateway::Gateway;
use snapfaas::workerpool;

use std::sync::Arc;

use time::precise_time_ns;
use signal_hook::{iterator::Signals, SIGINT};
use crossbeam_channel::bounded;

fn main() {
    simple_logger::init().expect("simple_logger init failed");

    let matches = App::new("SnapFaaS controller")
        .version("1.0")
        .author("David H. Liu <hao.liu@princeton.edu>")
        .about("Launch and configure SnapFaaS controller")
        .arg(
            Arg::with_name("config")
                .short("c")
                .long("config")
                .takes_value(true)
                .required(true)
                .help("Path to controller config YAML file"),
        )
        .arg(
            Arg::with_name("requests file")
                .long("requests_file")
                .takes_value(true)
                .required_unless_one(&["port number"])
                .conflicts_with("port number")
                .help("File containing JSON-lines of requests"),
        )
        .arg(
            Arg::with_name("port number")
                .long("port")
                .short("p")
                .takes_value(true)
                .required_unless_one(&["requests file"])
                .conflicts_with("requests file")
                .help("Port on which SnapFaaS accepts requests"),
        )
        .arg(Arg::with_name("total memory")
                .long("mem")
                .takes_value(true)
                .required(true)
                .help("Total memory available for all VMs")
        )
        .get_matches();

    // Fail ASAP
    if matches.value_of("requests file").is_none() && matches.value_of("port number").is_none() {
        panic!("no request file or port number specified");
    }

    // populate the in-memory config struct
    let config_path = matches.value_of("config").unwrap();
    let ctr_config = configs::ControllerConfig::new(config_path);

    // create a controller object
    let mut controller = Controller::new(ctr_config).expect("Cannot create controller");

    // set total memory
    let total_mem = matches.value_of("total memory").unwrap()
        .parse::<usize>().expect("Total memory is not a valid integer");
    controller.set_total_mem(total_mem);
    let controller = Arc::new(controller);
    trace!("{:?}", controller);

    let wp = workerpool::WorkerPool::new(controller.clone());
    trace!("# workers: {:?}", wp.pool_size());

    // register signal handler
    let (sig_sender, sig_receiver) = bounded(100);
    let signals = Signals::new(&[SIGINT]).expect("cannot create signals");
    std::thread::spawn(move || {
        for sig in signals.forever() {
            warn!("Received signal {:?}", sig);
            let _ = sig_sender.send(());
        }
    });


    // File Gateway
    if let Some(request_file_url) = matches.value_of("requests file") {
        let mut gateway = gateway::FileGateway::listen(request_file_url).expect("Failed to create file gateway");
        // start admitting and processing incoming requests
        let mut t1;
        let mut t2;
        let mut prev_ts = 0;
        loop {
            t1 = precise_time_ns();
            let task = gateway.next();
            match task {
                None => break,
                Some(task) => {
                    t2 = precise_time_ns();
                    info!("file gateway latency (t2-t1): {} ns", t2 - t1);
                    // ignore invalid requests
                    if task.is_err() {
                        error!("Invalid task: {:?}", task);
                        continue;
                    }

                    let (req, rsp_sender) = task.unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(req.time-prev_ts));
                    t1 = precise_time_ns();
                    prev_ts = req.time;

                    wp.send_req(req, rsp_sender);
                }
            }

            // check if received any signal
            if let Ok(_) = sig_receiver.try_recv() {
                warn!("snapctr shutdown received");
                wp.shutdown();
                std::process::exit(0);
            }
            t2 = precise_time_ns();
            info!("schedule latency (t2-t1): {} ns", t2-t1);
        }

        t2 = precise_time_ns();
        info!("gateway latency {:?} ns", t2-t1);

        wp.shutdown();
        println!("Shutting down...");
        std::process::exit(0);
    }

    // TCP gateway
    if let Some(p) = matches.value_of("port number") {
        let mut gateway = gateway::HTTPGateway::listen(p).expect("Failed to create HTTP gateway");
        info!("Gateway started on port: {:?}", gateway.port);

        loop {
            // read a request from TcpStreams and process it
            let task = gateway.next();
            match task {
                None=> (),
                Some(task) => {
                    // ignore invalid requests
                    if task.is_err() {
                        warn!("Invalid task: {:?}", task);
                        continue;
                    }

                    let (req, rsp_sender) = task.unwrap();

                    trace!("Gateway received request: {:?}. From: {:?}", req, rsp_sender);
                    wp.send_req_tcp(req, rsp_sender);
                }
            }

            // check if received any signal
            if let Ok(_) = sig_receiver.try_recv() {
                warn!("snapctr shutdown received");
                wp.shutdown();
                std::process::exit(0);
            }
        }
    }
}
