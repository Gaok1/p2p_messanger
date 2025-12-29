mod cli;
mod net;
mod profiles;
mod ui;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = cli::parse_args();
    let (net_tx, net_rx, net_handle) =
        net::start_network(args.bind_addr, args.peer_addr, args.autotune);

    let mut terminal = ui::setup_terminal()?;
    let mut app = ui::AppState::new(args.bind_addr, args.peer_addr, args.peer_label);
    let run_result = ui::run_app(&mut terminal, &mut app, net_tx.clone(), net_rx);
    ui::restore_terminal(&mut terminal)?;

    let _ = net_tx.send(net::NetCommand::Shutdown);
    let _ = net_handle.join();

    if let Err(err) = run_result {
        eprintln!("error: {err}");
    }

    Ok(())
}
