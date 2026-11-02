//! SBX-style effects, approximated with a PipeWire filter-chain sink placed in
//! front of the X4's sink. On Windows these effects run in Creative's driver on
//! the PC, not on the X4, so this is the Linux counterpart rather than a port.
//!
//! The graph only uses plugins that exist in PipeWire 1.0 (Ubuntu 24.04), plus
//! the LSP stereo compressor for Smart Volume when lsp-plugins-lv2 is installed
//! and PipeWire was built with LV2 support (Ubuntu's 1.0 build isn't).

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use gtk::glib;
use serde::{Deserialize, Serialize};

pub const SINK_NAME: &str = "sbx4_effects";
const SINK_DESCRIPTION: &str = "Sound Blaster X4 (SBX)";
const X4_SINK_PREFIX: &str = "alsa_output.usb-Creative_Technology_Ltd_Sound_Blaster_X4";
const COMPRESSOR_URI: &str = "http://lsp-plug.in/plugins/lv2/compressor_stereo";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Surround,
    Dialog,
    Bass,
    Crystalizer,
    SmartVolume,
}

pub const EFFECTS: [Kind; 5] = [Kind::Surround, Kind::Dialog, Kind::Bass, Kind::Crystalizer, Kind::SmartVolume];

impl Kind {
    pub fn title(self) -> &'static str {
        match self {
            Kind::Surround => "Surround",
            Kind::Dialog => "Dialog+",
            Kind::Bass => "Bass",
            Kind::Crystalizer => "Crystalizer",
            Kind::SmartVolume => "Smart Volume",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Kind::Surround => "Widens the stereo image; bass stays centered",
            Kind::Dialog => "Lifts voices in the center of the mix",
            Kind::Bass => "Adds low-end weight below 90 Hz",
            Kind::Crystalizer => "Adds treble harmonics and air for crisper detail",
            Kind::SmartVolume => "Evens out loud and quiet passages",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Effect {
    pub on: bool,
    /// 0.0–1.0
    pub level: f64,
}

/// Bass management for the X4's subwoofer output (2.1 profile).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Sub {
    pub on: bool,
    /// Crossover frequency, Hz.
    pub crossover: f64,
    /// Sub level, dB.
    pub level: f64,
    /// Take the bass below the crossover out of the speakers.
    pub high_pass: bool,
    pub invert: bool,
}

impl Default for Sub {
    fn default() -> Self {
        Self { on: false, crossover: 80.0, level: 0.0, high_pass: true, invert: false }
    }
}

/// Speed of sound, ft/s and m/s.
const SOUND_FT: f64 = 1125.0;
const SOUND_M: f64 = 343.0;
pub const CUTS: usize = 3;

/// One room EQ cut: a peaking filter set to pull a resonance down.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Cut {
    pub on: bool,
    pub freq: f64,
    /// dB, 0 or negative.
    pub gain: f64,
    pub q: f64,
}

impl Default for Cut {
    fn default() -> Self {
        Self { on: false, freq: 60.0, gain: -6.0, q: 4.0 }
    }
}

/// Room EQ: cuts at the room's bass resonances, predicted from its size and
/// tuned by ear with the test tones.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Room {
    pub on: bool,
    /// Length, width, height, in feet or meters.
    pub size: [f64; 3],
    pub metric: bool,
    pub cuts: [Cut; CUTS],
}

impl Default for Room {
    fn default() -> Self {
        Self { on: false, size: [12.0, 10.0, 8.0], metric: false, cuts: [Cut::default(); CUTS] }
    }
}

/// A predicted room resonance.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mode {
    pub freq: f64,
    /// How many dimensions resonate here (within 4%); coinciding modes boom most.
    pub strength: usize,
}

impl Room {
    /// Axial modes below `max_hz`, lowest first, with coinciding ones merged.
    pub fn modes(&self, max_hz: f64) -> Vec<Mode> {
        let sound = if self.metric { SOUND_M } else { SOUND_FT };
        let mut freqs: Vec<f64> = self
            .size
            .iter()
            .filter(|&&d| d > 0.0)
            .flat_map(|&d| (1..).map(move |n| f64::from(n) * sound / (2.0 * d)).take_while(move |&f| f <= max_hz))
            .collect();
        freqs.sort_by(f64::total_cmp);
        let mut modes: Vec<Mode> = Vec::new();
        for f in freqs {
            match modes.last_mut() {
                Some(m) if f / m.freq < 1.04 => {
                    m.freq = (m.freq * m.strength as f64 + f) / (m.strength as f64 + 1.0);
                    m.strength += 1;
                }
                _ => modes.push(Mode { freq: f, strength: 1 }),
            }
        }
        modes
    }

    /// Cuts for the strongest modes below 150 Hz (lowest first on ties), off
    /// for any left over.
    pub fn suggest_cuts(&self) -> [Cut; CUTS] {
        let mut modes = self.modes(150.0);
        modes.sort_by(|a, b| b.strength.cmp(&a.strength).then(a.freq.total_cmp(&b.freq)));
        let mut picked: Vec<Mode> = modes.into_iter().take(CUTS).collect();
        picked.sort_by(|a, b| a.freq.total_cmp(&b.freq));
        std::array::from_fn(|i| match picked.get(i) {
            Some(m) => Cut { on: true, freq: (m.freq * 2.0).round() / 2.0, gain: if m.strength > 1 { -8.0 } else { -6.0 }, q: 4.0 },
            None => Cut::default(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub enabled: bool,
    pub effects: [Effect; 5],
    #[serde(default)]
    pub sub: Sub,
    #[serde(default)]
    pub room: Room,
    /// The X4 is on its headphone output; the crossover is bypassed. Not saved.
    #[serde(skip)]
    pub headphones: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { enabled: false, effects: [Effect { on: false, level: 0.5 }; 5], sub: Sub::default(), room: Room::default(), headphones: false }
    }
}

impl Settings {
    pub fn effect(&self, kind: Kind) -> Effect {
        self.effects[kind as usize]
    }

    pub fn effect_mut(&mut self, kind: Kind) -> &mut Effect {
        &mut self.effects[kind as usize]
    }

    /// Whether the filter should run: for the effects, the subwoofer or room EQ.
    pub fn wants_filter(&self) -> bool {
        self.enabled || self.sub.on || self.room.on
    }

    /// The effect's level, or 0 when it or SBX is switched off.
    fn level(&self, kind: Kind) -> f64 {
        let e = self.effect(kind);
        if e.on && self.enabled {
            e.level
        } else {
            0.0
        }
    }
}

fn config_dir() -> PathBuf {
    glib::user_config_dir().join("sbx4-control")
}

pub fn load() -> Settings {
    fs::read(config_dir().join("sbx.json"))
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

pub fn save(settings: &Settings) -> io::Result<()> {
    fs::create_dir_all(config_dir())?;
    fs::write(config_dir().join("sbx.json"), serde_json::to_vec_pretty(settings)?)
}

/// Whether the PipeWire command-line tools the engine drives are installed.
pub fn tools_available() -> bool {
    ["pipewire", "pw-cli", "pw-dump", "wpctl"].iter().all(|tool| glib::find_program_in_path(tool).is_some())
}

/// Whether the LSP compressor used for Smart Volume is installed. PipeWire
/// may still be unable to load it; [`Engine::disable_compressor`] covers that.
pub fn compressor_available() -> bool {
    let mut dirs: Vec<PathBuf> = std::env::var("LV2_PATH")
        .map(|p| p.split(':').map(PathBuf::from).collect())
        .unwrap_or_default();
    dirs.push(glib::home_dir().join(".lv2"));
    dirs.extend(["/usr/lib/lv2", "/usr/lib/x86_64-linux-gnu/lv2", "/usr/local/lib/lv2"].map(PathBuf::from));
    dirs.iter().any(|d| d.join("lsp-plugins.lv2/compressor_stereo.ttl").exists())
}

/// A running filter-chain process and the id of the sink it created.
pub struct Engine {
    child: Option<Child>,
    node_id: Option<u32>,
    compressor: bool,
    /// The X4 sink the filter feeds has an LFE channel.
    lfe: bool,
}

impl Engine {
    pub fn new() -> Self {
        Self { child: None, node_id: None, compressor: compressor_available(), lfe: false }
    }

    pub fn has_compressor(&self) -> bool {
        self.compressor
    }

    /// Build the graph without the LV2 compressor from now on, for a PipeWire
    /// that can't load LV2 plugins.
    pub fn disable_compressor(&mut self) {
        self.compressor = false;
    }

    /// Process id of the filter, if one was started.
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    pub fn is_running(&mut self) -> bool {
        match &mut self.child {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }

    /// Start the filter process feeding `x4_sink`. Its sink appears shortly
    /// after; wait for it with [`wait_for_sink`] off the main thread. Its
    /// errors go to [`log_path`].
    pub fn spawn(&mut self, settings: &Settings, x4_sink: &Sink) -> io::Result<()> {
        self.stop();
        self.lfe = x4_sink.lfe;
        let path = config_dir().join("sbx-filter.conf");
        fs::create_dir_all(config_dir())?;
        fs::write(&path, config(settings, self.compressor, &x4_sink.name, self.lfe))?;

        let log = log_path();
        if let Some(dir) = log.parent() {
            fs::create_dir_all(dir)?;
        }
        let log = fs::File::create(log)?;

        let mut cmd = Command::new("pipewire");
        cmd.arg("-c").arg(&path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(log);
        // Take the filter down with us if the app dies. Must be spawned from the
        // main thread: the signal fires when the spawning *thread* exits.
        unsafe {
            cmd.pre_exec(|| {
                nix::sys::prctl::set_pdeathsig(nix::sys::signal::Signal::SIGTERM).map_err(io::Error::from)
            });
        }
        self.child = Some(cmd.spawn()?);
        Ok(())
    }

    pub fn set_node(&mut self, id: u32) {
        self.node_id = Some(id);
    }

    /// Push all control values to the running filter.
    pub fn update(&self, settings: &Settings) {
        let Some(id) = self.node_id else { return };
        let mut cmd = Command::new("pw-cli");
        cmd.args(["set-param", &id.to_string(), "Props", &params(settings, self.compressor, self.lfe)])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Ok(mut child) = cmd.spawn() {
            thread::spawn(move || child.wait());
        }
    }

    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.node_id = None;
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop();
    }
}

pub struct Sink {
    pub id: u32,
    pub name: String,
    /// Has a subwoofer (LFE) channel.
    pub lfe: bool,
}

/// The X4's own sink. Blocks briefly on `pw-dump`.
pub fn x4_sink() -> Option<Sink> {
    find_sink(|name| name.starts_with(X4_SINK_PREFIX) && plain_name(name))
}

/// The sink's name goes into the filter's config file inside quotes; only
/// accept the characters PipeWire uses for ALSA node names, so a crafted
/// name can't add settings of its own.
fn plain_name(name: &str) -> bool {
    name.chars().all(|c| c.is_ascii_alphanumeric() || "_.:-".contains(c))
}

/// Where the running filter writes its errors.
pub fn log_path() -> PathBuf {
    glib::user_cache_dir().join("sbx4-control").join("filter.log")
}

/// Why the filter's sink didn't appear.
pub enum StartError {
    /// The filter process quit, with the errors it logged.
    Exited(String),
    TimedOut,
}

impl StartError {
    /// The filter quit because PipeWire couldn't load an LV2 plugin.
    pub fn is_lv2(&self) -> bool {
        matches!(self, Self::Exited(errors) if errors.contains("lv2"))
    }
}

/// Wait for the sink of the filter running as `pid` to appear; returns its id.
/// Gives up early if the filter quits.
pub fn wait_for_sink(pid: u32, timeout: Duration) -> Result<u32, StartError> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Some(sink) = find_sink(|name| name == SINK_NAME) {
            return Ok(sink.id);
        }
        if has_exited(pid) {
            return Err(StartError::Exited(logged_errors()));
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(StartError::TimedOut)
}

/// Whether the child `pid` has quit, without reaping it: the main thread's
/// [`Engine`] still owns it and collects it with `wait`.
fn has_exited(pid: u32) -> bool {
    use nix::sys::wait::{waitid, Id, WaitPidFlag, WaitStatus};
    let flags = WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT;
    !matches!(waitid(Id::Pid(nix::unistd::Pid::from_raw(pid as i32)), flags), Ok(WaitStatus::StillAlive))
}

/// The error lines from the filter's log, without PipeWire's prefixes, e.g.
/// `can't load plugin type 'lv2': No such file or directory`.
fn logged_errors() -> String {
    let log = fs::read_to_string(log_path()).unwrap_or_default();
    let errors: Vec<&str> = log
        .lines()
        .filter(|line| line.starts_with("[E]"))
        .map(|line| line.rsplit_once("] ").map_or(line, |(_, msg)| msg).trim())
        .collect();
    errors.join("\n")
}

pub fn set_default_sink(id: u32) {
    let _ = Command::new("wpctl")
        .args(["set-default", &id.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn find_sink(matches: impl Fn(&str) -> bool) -> Option<Sink> {
    let output = Command::new("pw-dump").stderr(Stdio::null()).output().ok()?;
    let objects: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).ok()?;
    objects.iter().find_map(|o| {
        let props = o.pointer("/info/props")?;
        let name = props.get("node.name")?.as_str()?;
        let is_sink = props.get("media.class")?.as_str()? == "Audio/Sink";
        let lfe = props.get("audio.position").and_then(|p| p.as_str()).is_some_and(|p| p.split(',').any(|c| c == "LFE"));
        (is_sink && matches(name)).then(|| Some(Sink { id: o.get("id")?.as_u64()? as u32, name: name.to_owned(), lfe }))?
    })
}

fn db_to_gain(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

struct Node {
    name: String,
    /// Builtin label, or the LV2 URI for the compressor.
    plugin: &'static str,
    controls: Vec<(&'static str, f64)>,
}

fn node(name: impl Into<String>, plugin: &'static str, controls: &[(&'static str, f64)]) -> Node {
    Node { name: name.into(), plugin, controls: controls.to_vec() }
}

/// Build the filter graph for `s`: (nodes, links, graph outputs).
///
/// ```text
/// in_l/in_r → mid/side → Dialog+ (mid peaking) + Surround (high-passed side boost)
///   → per channel: Bass (low shelf) → Crystalizer (exciter: HPF → x² → HPF, mixed in,
///     plus an air shelf) → headroom trim → Smart Volume (compressor)
///   → Room EQ (peaking cuts)
///   → with `lfe`: Linkwitz-Riley crossover, (L+R)/2 low-passed to the sub
/// ```
fn graph(s: &Settings, compressor: bool, lfe: bool) -> (Vec<Node>, Vec<(String, String)>, Vec<String>) {
    let width = s.level(Kind::Surround) * 1.5;
    let dialog_db = s.level(Kind::Dialog) * 9.0;
    let bass_db = s.level(Kind::Bass) * 10.0;
    let crystal = s.level(Kind::Crystalizer);
    let (exciter, air_db) = (crystal * 8.0, crystal * 4.0);
    // Pull the level down as much as the boosts push it up, so they don't clip.
    let headroom_db = [bass_db * 0.8, dialog_db * 0.6, air_db * 0.5 + crystal * 1.5, width * 3.0]
        .into_iter()
        .fold(0.0, f64::max);

    let mut nodes = vec![
        node("in_l", "copy", &[]),
        node("in_r", "copy", &[]),
        node("mid", "mixer", &[("Gain 1", 0.5), ("Gain 2", 0.5)]),
        node("inv_r", "invert", &[]),
        node("side", "mixer", &[("Gain 1", 0.5), ("Gain 2", 0.5)]),
        node("dialog", "bq_peaking", &[("Freq", 2500.0), ("Q", 0.8), ("Gain", dialog_db)]),
        node("side_hp", "bq_highpass", &[("Freq", 150.0), ("Q", 0.707)]),
        node("inv_side", "invert", &[]),
        node("inv_side_hp", "invert", &[]),
        node("ms_l", "mixer", &[("Gain 1", 1.0), ("Gain 2", 1.0), ("Gain 3", width)]),
        node("ms_r", "mixer", &[("Gain 1", 1.0), ("Gain 2", 1.0), ("Gain 3", width)]),
    ];
    let mut links: Vec<(String, String)> = [
        ("in_l:Out", "mid:In 1"),
        ("in_r:Out", "mid:In 2"),
        ("in_r:Out", "inv_r:In"),
        ("in_l:Out", "side:In 1"),
        ("inv_r:Out", "side:In 2"),
        ("mid:Out", "dialog:In"),
        ("side:Out", "side_hp:In"),
        ("side:Out", "inv_side:In"),
        ("side_hp:Out", "inv_side_hp:In"),
        // L = mid + side + width·hp(side); R = mid − side − width·hp(side)
        ("dialog:Out", "ms_l:In 1"),
        ("side:Out", "ms_l:In 2"),
        ("side_hp:Out", "ms_l:In 3"),
        ("dialog:Out", "ms_r:In 1"),
        ("inv_side:Out", "ms_r:In 2"),
        ("inv_side_hp:Out", "ms_r:In 3"),
    ]
    .iter()
    .map(|(a, b)| (a.to_string(), b.to_string()))
    .collect();

    for ch in ["l", "r"] {
        nodes.extend([
            node(format!("bass_{ch}"), "bq_lowshelf", &[("Freq", 90.0), ("Q", 0.707), ("Gain", bass_db)]),
            node(format!("exc_hp_{ch}"), "bq_highpass", &[("Freq", 3000.0), ("Q", 0.707)]),
            node(format!("exc_sq_{ch}"), "mult", &[]),
            node(format!("exc_dc_{ch}"), "bq_highpass", &[("Freq", 2000.0), ("Q", 0.707)]),
            node(format!("cry_{ch}"), "mixer", &[("Gain 1", 1.0), ("Gain 2", exciter)]),
            node(format!("air_{ch}"), "bq_highshelf", &[("Freq", 8000.0), ("Q", 0.707), ("Gain", air_db)]),
            node(format!("trim_{ch}"), "mixer", &[("Gain 1", db_to_gain(-headroom_db))]),
        ]);
        for (from, to) in [
            ("ms:Out", "bass:In"),
            ("bass:Out", "exc_hp:In"),
            ("exc_hp:Out", "exc_sq:In 1"),
            ("exc_hp:Out", "exc_sq:In 2"),
            ("exc_sq:Out", "exc_dc:In"),
            ("bass:Out", "cry:In 1"),
            ("exc_dc:Out", "cry:In 2"),
            ("cry:Out", "air:In"),
            ("air:Out", "trim:In 1"),
        ] {
            let channel = |port: &str| port.replacen(':', &format!("_{ch}:"), 1);
            links.push((channel(from), channel(to)));
        }
    }

    let stereo = if compressor {
        let smart = s.effect(Kind::SmartVolume);
        let threshold_db = -12.0 - smart.level * 18.0;
        let ratio = 2.0 + smart.level * 4.0;
        let makeup_db = -threshold_db * (1.0 - 1.0 / ratio) * 0.6;
        nodes.push(node(
            "comp",
            COMPRESSOR_URI,
            &[
                ("enabled", if smart.on && s.enabled { 1.0 } else { 0.0 }),
                ("al", db_to_gain(threshold_db)),
                ("cr", ratio),
                ("at", 10.0),
                ("rt", 300.0),
                ("mk", db_to_gain(makeup_db)),
            ],
        ));
        links.push(("trim_l:Out".into(), "comp:in_l".into()));
        links.push(("trim_r:Out".into(), "comp:in_r".into()));
        ["comp:out_l", "comp:out_r"]
    } else {
        ["trim_l:Out", "trim_r:Out"]
    };

    // Room EQ: the room booms whichever speaker plays the note, so the cuts go
    // before the crossover. Headphones don't hear the room.
    let room_active = s.room.on && !s.headphones;
    let mut stereo = stereo.map(String::from);
    for (i, cut) in s.room.cuts.iter().enumerate() {
        let gain = if room_active && cut.on { cut.gain } else { 0.0 };
        for (ch, input) in ["l", "r"].into_iter().zip(&mut stereo) {
            let name = format!("room{}_{ch}", i + 1);
            nodes.push(node(&name, "bq_peaking", &[("Freq", cut.freq), ("Q", cut.q), ("Gain", gain)]));
            links.push((std::mem::replace(input, format!("{name}:Out")), format!("{name}:In")));
        }
    }
    if !lfe {
        return (nodes, links, stereo.to_vec());
    }

    // Two cascaded Butterworth sections per side make a 4th-order
    // Linkwitz-Riley crossover, whose halves sum flat. Switching it on and off
    // only changes gains, so it never needs a restart.
    let sub = s.sub;
    let active = sub.on && !s.headphones;
    let xo = sub.crossover;
    let high_pass = if active && sub.high_pass { 1.0 } else { 0.0 };
    let polarity = if sub.invert { -1.0 } else { 1.0 };
    let sub_gain = if active { polarity * db_to_gain(sub.level) } else { 0.0 };
    for (ch, input) in ["l", "r"].into_iter().zip(&stereo) {
        nodes.extend([
            node(format!("xo_hp1_{ch}"), "bq_highpass", &[("Freq", xo), ("Q", 0.707)]),
            node(format!("xo_hp2_{ch}"), "bq_highpass", &[("Freq", xo), ("Q", 0.707)]),
            // Gain 1: high-passed, Gain 2: full range.
            node(format!("main_{ch}"), "mixer", &[("Gain 1", high_pass), ("Gain 2", 1.0 - high_pass)]),
        ]);
        links.extend([
            (input.to_owned(), format!("xo_hp1_{ch}:In")),
            (format!("xo_hp1_{ch}:Out"), format!("xo_hp2_{ch}:In")),
            (format!("xo_hp2_{ch}:Out"), format!("main_{ch}:In 1")),
            (input.to_owned(), format!("main_{ch}:In 2")),
        ]);
    }
    nodes.extend([
        node("sub_sum", "mixer", &[("Gain 1", 0.5), ("Gain 2", 0.5)]),
        node("xo_lp1", "bq_lowpass", &[("Freq", xo), ("Q", 0.707)]),
        node("xo_lp2", "bq_lowpass", &[("Freq", xo), ("Q", 0.707)]),
        node("sub_out", "mixer", &[("Gain 1", sub_gain)]),
    ]);
    for (from, to) in [
        (stereo[0].as_str(), "sub_sum:In 1"),
        (stereo[1].as_str(), "sub_sum:In 2"),
        ("sub_sum:Out", "xo_lp1:In"),
        ("xo_lp1:Out", "xo_lp2:In"),
        ("xo_lp2:Out", "sub_out:In 1"),
    ] {
        links.push((from.to_owned(), to.to_owned()));
    }
    (nodes, links, vec!["main_l:Out".into(), "main_r:Out".into(), "sub_out:Out".into()])
}

fn config(s: &Settings, compressor: bool, x4_sink: &str, lfe: bool) -> String {
    let (nodes, links, outputs) = graph(s, compressor, lfe);
    let positions = if lfe { "FL FR LFE" } else { "FL FR" };
    let mut c = String::from(
        "context.properties = { log.level = 2 }\n\
         context.spa-libs = {\n    audio.convert.* = audioconvert/libspa-audioconvert\n    support.* = support/libspa-support\n}\n\
         context.modules = [\n    { name = libpipewire-module-rt args = { } flags = [ ifexists nofail ] }\n    \
         { name = libpipewire-module-protocol-native }\n    { name = libpipewire-module-client-node }\n    \
         { name = libpipewire-module-adapter }\n    { name = libpipewire-module-filter-chain\n        args = {\n",
    );
    let _ = writeln!(c, "            node.description = \"{SINK_DESCRIPTION}\"\n            media.name = \"{SINK_DESCRIPTION}\"");
    c.push_str("            filter.graph = {\n                nodes = [\n");
    for n in &nodes {
        let kind = if n.plugin.contains("://") {
            format!("type = lv2 name = {} plugin = \"{}\"", n.name, n.plugin)
        } else {
            format!("type = builtin name = {} label = {}", n.name, n.plugin)
        };
        let controls: String = n.controls.iter().map(|(k, v)| format!(" \"{k}\" = {v:.6}")).collect();
        let _ = writeln!(c, "                    {{ {kind} control = {{{controls} }} }}");
    }
    c.push_str("                ]\n                links = [\n");
    for (from, to) in &links {
        let _ = writeln!(c, "                    {{ output = \"{from}\" input = \"{to}\" }}");
    }
    let outputs: String = outputs.iter().map(|o| format!(" \"{o}\"")).collect();
    let _ = writeln!(
        c,
        "                ]\n                inputs = [ \"in_l:In\" \"in_r:In\" ]\n                outputs = [{outputs} ]\n            }}"
    );
    // The output is pinned to the X4: tools that grab every stream (EasyEffects)
    // would otherwise move it into themselves, or onto another sink while the
    // X4 restarts, and lose the LFE channel.
    let _ = writeln!(
        c,
        "            capture.props = {{\n                node.name = \"{SINK_NAME}\"\n                media.class = Audio/Sink\n                audio.position = [ FL FR ]\n            }}\n            \
         playback.props = {{\n                node.name = \"{SINK_NAME}.output\"\n                node.passive = true\n                audio.position = [ {positions} ]\n                target.object = \"{x4_sink}\"\n                node.dont-move = true\n                node.dont-reconnect = true\n            }}\n        }}\n    }}\n]"
    );
    c
}

/// All controls as a `pw-cli set-param … Props` argument.
fn params(s: &Settings, compressor: bool, lfe: bool) -> String {
    let (nodes, _, _) = graph(s, compressor, lfe);
    let mut p = String::from("{ params = [");
    for n in &nodes {
        for (k, v) in &n.controls {
            let _ = write!(p, " \"{}:{k}\" {v:.6}", n.name);
        }
    }
    p.push_str(" ] }");
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_settings_are_transparent() {
        let (nodes, _, outputs) = graph(&Settings::default(), true, true);
        let control = |node: &str, port: &str| {
            nodes.iter().find(|n| n.name == node).unwrap().controls.iter().find(|(k, _)| *k == port).unwrap().1
        };
        assert_eq!(control("ms_l", "Gain 3"), 0.0);
        assert_eq!(control("dialog", "Gain"), 0.0);
        assert_eq!(control("bass_r", "Gain"), 0.0);
        assert_eq!(control("cry_l", "Gain 2"), 0.0);
        assert_eq!(control("trim_l", "Gain 1"), 1.0);
        assert_eq!(control("comp", "enabled"), 0.0);
        assert_eq!(outputs.len(), 3);
        assert_eq!(control("main_l", "Gain 2"), 1.0);
        assert_eq!(control("sub_out", "Gain 1"), 0.0);
    }

    #[test]
    fn room_modes_for_a_small_room() {
        let room = Room { size: [10.0, 8.0, 8.0], ..Room::default() };
        let modes = room.modes(150.0);
        // 56.25 (10 ft), 70.3 (8 ft, twice), 112.5 (10 ft, 2nd), 140.6 (8 ft, 2nd, twice)
        assert_eq!(modes.len(), 4, "{modes:?}");
        assert!((modes[0].freq - 56.25).abs() < 0.1 && modes[0].strength == 1);
        assert!((modes[1].freq - 70.31).abs() < 0.1 && modes[1].strength == 2);
        let cuts = room.suggest_cuts();
        assert!(cuts.iter().all(|c| c.on));
        assert_eq!(cuts.map(|c| c.freq), [56.5, 70.5, 140.5]);
        assert_eq!(cuts[1].gain, -8.0);
    }

    #[test]
    fn room_eq_is_neutral_until_switched_on() {
        let mut s = Settings::default();
        s.room.cuts = s.room.suggest_cuts();
        let gain = |s: &Settings| {
            let (nodes, _, _) = graph(s, true, true);
            nodes.iter().find(|n| n.name == "room1_l").unwrap().controls[2].1
        };
        assert_eq!(gain(&s), 0.0);
        s.room.on = true;
        assert!(s.wants_filter());
        assert!(gain(&s) < 0.0);
        s.headphones = true;
        assert_eq!(gain(&s), 0.0);
    }

    #[test]
    fn effects_stay_neutral_when_only_the_sub_is_on() {
        let mut s = Settings::default();
        s.effects.iter_mut().for_each(|e| e.on = true);
        s.sub.on = true;
        assert!(s.wants_filter());
        let (nodes, _, _) = graph(&s, true, true);
        let control = |node: &str, port: &str| {
            nodes.iter().find(|n| n.name == node).unwrap().controls.iter().find(|(k, _)| *k == port).unwrap().1
        };
        assert_eq!(control("bass_l", "Gain"), 0.0);
        assert_eq!(control("comp", "enabled"), 0.0);
        assert_eq!(control("main_r", "Gain 1"), 1.0);
        assert_eq!(control("sub_out", "Gain 1"), 1.0);

        s.headphones = true;
        let (nodes, _, _) = graph(&s, true, true);
        let gain = nodes.iter().find(|n| n.name == "sub_out").unwrap().controls[0].1;
        assert_eq!(gain, 0.0);
    }

    #[test]
    fn every_link_names_existing_nodes() {
        let mut s = Settings::default();
        s.effects.iter_mut().for_each(|e| e.on = true);
        for lfe in [false, true] {
            let (nodes, links, outputs) = graph(&s, true, lfe);
            let exists = |port: &str| nodes.iter().any(|n| n.name == port.split(':').next().unwrap());
            assert!(links.iter().all(|(a, b)| exists(a) && exists(b)));
            assert!(outputs.iter().all(|o| exists(o)));
        }
        let c = config(&s, true, "x4", true);
        assert!(c.contains("target.object = \"x4\""));
        assert!(c.contains("audio.position = [ FL FR LFE ]"));
    }

    #[test]
    fn only_plain_sink_names_are_accepted() {
        assert!(plain_name("alsa_output.usb-Creative_Technology_Ltd_Sound_Blaster_X4_5E8EE3AC95BB9326-03.analog-surround-21"));
        assert!(!plain_name("x\" }] context.modules = [ { name = evil"));
        assert!(!plain_name("x4 sink"));
    }
}
