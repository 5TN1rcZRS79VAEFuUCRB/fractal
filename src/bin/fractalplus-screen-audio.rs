//! Helper for Fractal's screen sharing: capture the sound of other apps
//! through `PipeWire` and write it to stdout.
//!
//! This is a separate program because the WebRTC library linked into Fractal
//! defines its own `pw_*` functions, which shadow the real `PipeWire` ones.
//!
//! Usage:
//!
//! - `fractalplus-screen-audio list [EXCLUDED_PID]`: print the names of the apps
//!   playing sound, one per line.
//! - `fractalplus-screen-audio system [EXCLUDED_PID]`: capture every app.
//! - `fractalplus-screen-audio app NAME [EXCLUDED_PID]`: capture the app with the
//!   given name.
//!
//! The sound is written as interleaved stereo 32-bit float samples at 48 kHz.
//! The capture stops when stdin is closed.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io::{Read, Write},
    process::ExitCode,
    rc::Rc,
    thread,
};

use pipewire as pw;
use pw::{properties::properties, spa, types::ObjectType};

/// The sample rate of the captured sound.
const SAMPLE_RATE: u32 = 48_000;
/// The number of channels of the captured sound.
const CHANNELS: u32 = 2;

/// Which sound to capture.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
enum Audio {
    /// The sound of every app.
    #[default]
    System,
    /// Only the sound of the app with the given name.
    App(String),
}

fn main() -> ExitCode {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let (command, rest) = match args.split_first() {
        Some((command, rest)) => (command.as_str(), rest),
        None => ("", &[][..]),
    };
    let (audio, rest) = match command {
        "list" => (None, rest),
        "system" => (Some(Audio::System), rest),
        "app" if !rest.is_empty() => (Some(Audio::App(rest[0].clone())), &rest[1..]),
        _ => {
            eprintln!("Usage: fractalplus-screen-audio list|system|app NAME [EXCLUDED_PID]");
            return ExitCode::from(2);
        }
    };
    let mut excluded_pids = vec![std::process::id()];
    excluded_pids.extend(rest.first().and_then(|pid| pid.parse::<u32>().ok()));

    let result = match audio {
        None => list_apps(&excluded_pids).map(|apps| {
            for app in apps {
                println!("{app}");
            }
        }),
        Some(audio) => capture(&audio, excluded_pids),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

/// List the names of the apps playing sound.
fn list_apps(excluded_pids: &[u32]) -> Result<Vec<String>, String> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| e.to_string())?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| e.to_string())?;
    let core = context.connect_rc(None).map_err(|e| e.to_string())?;
    let registry = core.get_registry_rc().map_err(|e| e.to_string())?;

    let graph = Rc::new(RefCell::new(Graph {
        excluded_pids: excluded_pids.to_vec(),
        ..Default::default()
    }));
    let _listener = registry
        .add_listener_local()
        .global({
            let graph = graph.clone();
            let registry = registry.clone();
            move |global| add_global(&graph, &registry, None, global)
        })
        .register();
    // The first roundtrip gets the objects, the second what the clients report.
    roundtrip(&mainloop, &core);
    roundtrip(&mainloop, &core);

    let graph = graph.borrow();
    let names = graph
        .nodes
        .values()
        .filter(|node| node.is_playback && graph.is_ours(node) == Some(false))
        .filter_map(|node| node.app_name.clone())
        .collect::<HashSet<_>>();
    let mut names = names.into_iter().collect::<Vec<_>>();
    names.sort();
    Ok(names)
}

/// Capture the given sound to stdout until stdin is closed.
fn capture(audio: &Audio, excluded_pids: Vec<u32>) -> Result<(), String> {
    let (quit, quit_rx) = pw::channel::channel();
    // Quit when our parent closes stdin, or dies.
    thread::spawn(move || {
        let mut buf = [0; 64];
        while std::io::stdin().read(&mut buf).is_ok_and(|n| n > 0) {}
        let _ = quit.send(());
    });

    let mut stdout = std::io::stdout().lock();
    run_capture(
        audio,
        excluded_pids,
        move |samples: &[f32]| {
            let bytes = samples
                .iter()
                .flat_map(|sample| sample.to_le_bytes())
                .collect::<Vec<u8>>();
            stdout
                .write_all(&bytes)
                .and_then(|()| stdout.flush())
                .is_ok()
        },
        quit_rx,
    )
}

/// Run the capture until we are told to quit.
#[allow(clippy::too_many_lines)]
///
/// `on_samples` returns `false` to stop.
fn run_capture(
    audio: &Audio,
    excluded_pids: Vec<u32>,
    mut on_samples: impl FnMut(&[f32]) -> bool + 'static,
    quit_rx: pw::channel::Receiver<()>,
) -> Result<(), String> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| e.to_string())?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| e.to_string())?;
    let core = context.connect_rc(None).map_err(|e| e.to_string())?;
    let registry = core.get_registry_rc().map_err(|e| e.to_string())?;

    let _quit = quit_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });

    let node_name = format!("fractalplus-screen-audio-{}", std::process::id());
    let stream = pw::stream::StreamRc::new(
        core.clone(),
        "fractalplus-screen-audio",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::NODE_NAME => node_name.as_str(),
            *pw::keys::NODE_DESCRIPTION => "Fractal screen sharing",
        },
    )
    .map_err(|e| e.to_string())?;

    let _stream_listener = stream
        .add_local_listener_with_user_data(Vec::<f32>::new())
        .process({
            let mainloop = mainloop.clone();
            move |stream, samples| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let Some(data) = buffer.datas_mut().first_mut() else {
                    return;
                };
                let offset = data.chunk().offset() as usize;
                let size = data.chunk().size() as usize;
                let Some(bytes) = data.data() else {
                    return;
                };
                let Some(bytes) = bytes.get(offset..offset + size) else {
                    return;
                };
                samples.clear();
                samples.extend(
                    bytes
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|b| f32::from_le_bytes(*b)),
                );
                if !on_samples(samples) {
                    mainloop.quit();
                }
            }
        })
        .register()
        .map_err(|e| e.to_string())?;

    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
    audio_info.set_rate(SAMPLE_RATE);
    audio_info.set_channels(CHANNELS);
    let mut position = [0; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    audio_info.set_position(position);
    let format = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: spa::param::ParamType::EnumFormat.as_raw(),
            properties: audio_info.into(),
        }),
    )
    .map_err(|e| format!("{e:?}"))?
    .0
    .into_inner();
    let mut params = [spa::pod::Pod::from_bytes(&format).ok_or("invalid audio format")?];

    // Don't autoconnect: we choose what is linked to the stream.
    stream
        .connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| e.to_string())?;

    let graph = Rc::new(RefCell::new(Graph {
        audio: audio.clone(),
        excluded_pids,
        own_node_name: node_name,
        ..Default::default()
    }));
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let graph = graph.clone();
            let registry = registry.clone();
            let core = core.clone();
            move |global| {
                add_global(&graph, &registry, Some(&core), global);
                graph.borrow_mut().link_all(&core);
            }
        })
        .global_remove({
            let graph = graph.clone();
            move |id| graph.borrow_mut().remove_global(id)
        })
        .register();

    eprintln!("Capturing {audio:?}");
    mainloop.run();

    // Remove our links before disconnecting.
    graph.borrow_mut().links.clear();
    Ok(())
}

/// Remember a new object, and watch the properties of clients.
///
/// With a core, links are updated when a client's properties arrive.
fn add_global(
    graph: &Rc<RefCell<Graph>>,
    registry: &pw::registry::RegistryRc,
    core: Option<&pw::core::CoreRc>,
    global: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>,
) {
    graph.borrow_mut().add_global(global);
    if global.type_ != ObjectType::Client {
        return;
    }
    let Ok(client) = registry.bind::<pw::client::Client, _>(global) else {
        return;
    };
    let id = global.id;
    let listener = client
        .add_listener_local()
        .info({
            let graph = Rc::downgrade(graph);
            let core = core.cloned();
            move |info| {
                if let Some(graph) = graph.upgrade() {
                    let mut graph = graph.borrow_mut();
                    graph.set_client_info(id, info.props());
                    if let Some(core) = &core {
                        graph.link_all(core);
                    }
                }
            }
        })
        .register();
    graph
        .borrow_mut()
        .client_proxies
        .insert(id, (client, listener));
}

/// A client of `PipeWire`.
#[derive(Debug, Default)]
struct Client {
    /// The process ID seen by `PipeWire`.
    sec_pid: Option<u32>,
    /// The process ID the client reports, which differs from `sec_pid` in a
    /// sandbox like Flatpak.
    process_id: Option<u32>,
    /// The program the client reports.
    binary: Option<String>,
    /// Whether we got the properties the client reports.
    has_info: bool,
}

/// A node in the `PipeWire` graph.
#[derive(Debug, Default)]
struct Node {
    name: Option<String>,
    app_name: Option<String>,
    client_id: Option<u32>,
    /// Whether this is an app playing sound.
    is_playback: bool,
}

/// A port in the `PipeWire` graph.
#[derive(Debug)]
struct Port {
    node_id: u32,
    is_output: bool,
    channel: String,
}

/// What we know about the `PipeWire` graph, and the links we made.
#[derive(Default)]
struct Graph {
    audio: Audio,
    excluded_pids: Vec<u32>,
    own_node_name: String,
    clients: HashMap<u32, Client>,
    /// The proxies we bound to read the clients' properties.
    client_proxies: HashMap<u32, (pw::client::Client, pw::client::ClientListener)>,
    nodes: HashMap<u32, Node>,
    ports: HashMap<u32, Port>,
    /// Our links, by output and input port.
    links: HashMap<(u32, u32), pw::link::Link>,
}

impl Graph {
    /// Remember a new object.
    fn add_global(&mut self, global: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>) {
        let Some(props) = global.props else {
            return;
        };
        match global.type_ {
            ObjectType::Client => {
                self.clients.insert(
                    global.id,
                    Client {
                        sec_pid: props.get("pipewire.sec.pid").and_then(|p| p.parse().ok()),
                        ..Default::default()
                    },
                );
            }
            ObjectType::Node => {
                self.nodes.insert(
                    global.id,
                    Node {
                        name: props.get("node.name").map(ToOwned::to_owned),
                        app_name: props
                            .get("application.name")
                            .or_else(|| props.get("node.name"))
                            .map(ToOwned::to_owned),
                        client_id: props.get("client.id").and_then(|id| id.parse().ok()),
                        is_playback: props.get("media.class") == Some("Stream/Output/Audio"),
                    },
                );
            }
            ObjectType::Port => {
                let (Some(node_id), Some(direction)) = (
                    props.get("node.id").and_then(|id| id.parse().ok()),
                    props.get("port.direction"),
                ) else {
                    return;
                };
                self.ports.insert(
                    global.id,
                    Port {
                        node_id,
                        is_output: direction == "out",
                        channel: props.get("audio.channel").unwrap_or("MONO").to_owned(),
                    },
                );
            }
            _ => {}
        }
    }

    /// Remember the properties a client reports.
    fn set_client_info(&mut self, id: u32, props: Option<&spa::utils::dict::DictRef>) {
        let Some(client) = self.clients.get_mut(&id) else {
            return;
        };
        if let Some(props) = props {
            client.process_id = props
                .get("application.process.id")
                .and_then(|p| p.parse().ok());
            client.binary = props
                .get("application.process.binary")
                .map(ToOwned::to_owned);
        }
        client.has_info = true;
    }

    /// Forget a removed object.
    fn remove_global(&mut self, id: u32) {
        self.clients.remove(&id);
        self.client_proxies.remove(&id);
        self.nodes.remove(&id);
        if self.ports.remove(&id).is_some() {
            self.links
                .retain(|(output, input), _| *output != id && *input != id);
        }
    }

    /// Whether the given node belongs to Fractal, or to a client we don't
    /// know enough about yet.
    ///
    /// Returns `None` if we can't tell yet.
    fn is_ours(&self, node: &Node) -> Option<bool> {
        let client = self.clients.get(&node.client_id?)?;
        if client
            .sec_pid
            .is_some_and(|pid| self.excluded_pids.contains(&pid))
        {
            return Some(true);
        }
        if !client.has_info {
            return None;
        }
        // In a sandbox, `PipeWire` sees other process IDs than we do, so also
        // check what the client reports.
        let reported_ours = client
            .process_id
            .is_some_and(|pid| self.excluded_pids.contains(&pid))
            && client
                .binary
                .as_deref()
                .is_some_and(|binary| binary.starts_with("fractal"));
        Some(reported_ours)
    }

    /// Whether we should capture the given node.
    fn should_capture(&self, node: &Node) -> bool {
        if !node.is_playback {
            return false;
        }
        // If we can't tell yet whether it is us, wait.
        if self.is_ours(node) != Some(false) {
            return false;
        }
        match &self.audio {
            Audio::System => true,
            Audio::App(name) => node.app_name.as_ref() == Some(name),
        }
    }

    /// Link the ports of the apps to capture to our stream.
    fn link_all(&mut self, core: &pw::core::CoreRc) {
        let Some(own_node) = self
            .nodes
            .iter()
            .find(|(_, node)| node.name.as_deref() == Some(self.own_node_name.as_str()))
            .map(|(id, _)| *id)
        else {
            return;
        };

        let inputs = self
            .ports
            .iter()
            .filter(|(_, port)| port.node_id == own_node && !port.is_output)
            .map(|(id, port)| (*id, port.channel.clone()))
            .collect::<Vec<_>>();
        let outputs = self
            .ports
            .iter()
            .filter(|(_, port)| {
                port.is_output
                    && self
                        .nodes
                        .get(&port.node_id)
                        .is_some_and(|node| self.should_capture(node))
            })
            .map(|(id, port)| (*id, port.node_id, port.channel.clone()))
            .collect::<Vec<_>>();

        for (output, output_node, output_channel) in &outputs {
            for (input, input_channel) in &inputs {
                if self.links.contains_key(&(*output, *input))
                    || !channels_match(output_channel, input_channel)
                {
                    continue;
                }
                let link = core.create_object::<pw::link::Link>(
                    "link-factory",
                    &properties! {
                        "link.output.port" => output.to_string(),
                        "link.input.port" => input.to_string(),
                        "link.output.node" => output_node.to_string(),
                        "link.input.node" => own_node.to_string(),
                        "object.linger" => "false",
                    },
                );
                match link {
                    Ok(link) => {
                        self.links.insert((*output, *input), link);
                    }
                    Err(error) => eprintln!("Could not link app sound: {error}"),
                }
            }
        }
    }
}

/// Whether an app's output channel should go to one of our input channels.
fn channels_match(output: &str, input: &str) -> bool {
    let left = matches!(output, "FL" | "RL" | "SL" | "FLC" | "RLC");
    let right = matches!(output, "FR" | "RR" | "SR" | "FRC" | "RRC");
    match input {
        "FL" => !right,
        "FR" => !left,
        _ => false,
    }
}

/// Process events until the server handled everything we sent.
fn roundtrip(mainloop: &pw::main_loop::MainLoopRc, core: &pw::core::CoreRc) {
    let Ok(pending) = core.sync(0) else {
        return;
    };
    let _listener = core
        .add_listener_local()
        .done({
            let mainloop = mainloop.clone();
            move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == pending {
                    mainloop.quit();
                }
            }
        })
        .register();
    mainloop.run();
}
