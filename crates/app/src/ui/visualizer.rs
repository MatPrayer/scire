//! 3D audio visualizer for the fullscreen player.
//!
//! gpui exposes no shaders or 3D primitives, so the scenes are projected in
//! software: vertices are built in a right-handed world space (x right, y up,
//! z into the screen), divided through by depth, and painted as gpui paths and
//! quads. Everything is drawn back-to-front so nearer geometry occludes what is
//! behind it — there is no depth buffer to do it for us.
//!
//! Frequency data comes from `playback::spectrum`: the engine mirrors output
//! samples into a lock-free ring, and `tick` runs one FFT per frame.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use gpui::{
    Bounds, Hsla, PathBuilder, Pixels, Point, canvas, fill, linear_color_stop, linear_gradient,
    point, px,
};
use playback::spectrum::{self, FFT_SIZE, SpectrumTap};

use crate::config::VisualizerMode;

/// Frequency bands the spectrum is reduced to. Also the horizontal resolution
/// of the terrain and the angular resolution of the tunnel.
const BANDS: usize = 48;
/// Terrain rows kept in history (depth of the scrolling grid).
const ROWS: usize = 26;
/// How often a new terrain row is pushed, independent of frame rate.
const ROW_INTERVAL: f32 = 1. / 30.;
/// Points in the sphere cloud.
const POINTS: usize = 420;
/// Cross-fade between scenes. Short on purpose: the new scene should land on
/// the beat that triggered it, with the old one clearing out behind it.
const FADE: f32 = 0.22;

/// One of the drawable scenes. `VisualizerMode` adds Off and Auto on top; this
/// is what is actually on screen at a given moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scene {
    Terrain,
    Tunnel,
    Sphere,
    Retro,
    Orb,
}

impl Scene {
    const ALL: [Scene; 5] = [
        Scene::Terrain,
        Scene::Tunnel,
        Scene::Sphere,
        Scene::Retro,
        Scene::Orb,
    ];
}

/// The scene a mode pins, or `None` when the mode leaves the choice open (Off
/// draws nothing; Auto picks for itself).
fn pinned_scene(mode: VisualizerMode) -> Option<Scene> {
    match mode {
        VisualizerMode::Terrain => Some(Scene::Terrain),
        VisualizerMode::Tunnel => Some(Scene::Tunnel),
        VisualizerMode::Sphere => Some(Scene::Sphere),
        VisualizerMode::Retro => Some(Scene::Retro),
        VisualizerMode::Orb => Some(Scene::Orb),
        VisualizerMode::Off | VisualizerMode::Auto => None,
    }
}

/// Decides when Auto mode should move to the next scene, from the spectral
/// flux (how much the spectrum jumped since the last frame).
///
/// Switching on every beat would be seasickness; switching on a timer would
/// land in the middle of a phrase. So it waits out a hold, then goes on the
/// first flux peak that stands out from the recent past — a drop, a chorus, a
/// new section — and gives up and switches anyway if the track never provides
/// one.
pub struct OnsetSwitcher {
    /// Recent flux values, for the mean/spread the threshold rides on.
    hist: VecDeque<f32>,
    /// Recent low-end level, for the jump that confirms a hit.
    bass_hist: VecDeque<f32>,
    /// Seconds since the last switch.
    since: f32,
    /// Flux crossed the threshold and we are waiting for the hit itself.
    armed: Option<Armed>,
}

/// One frame of detection input.
#[derive(Debug, Clone, Copy, Default)]
pub struct Onset {
    /// Spectral flux across the whole spectrum.
    pub flux: f32,
    /// Spectral flux restricted to the low end — the kick and bass.
    pub bass_flux: f32,
    /// Mean band level, used only to gate out silence.
    pub energy: f32,
    /// Mean low-end level, for the jump that confirms a hit.
    pub bass: f32,
}

impl Onset {
    /// The detection function: mostly low end. Scene changes should land on
    /// the kick, so the bottom of the spectrum drives the decision and the
    /// rest only contributes.
    fn strength(&self) -> f32 {
        BASS_WEIGHT * self.bass_flux + (1. - BASS_WEIGHT) * self.flux
    }
}

/// Bookkeeping between "something is building" and "it landed".
struct Armed {
    waited: f32,
    peak_flux: f32,
    /// Frames since the flux peak — the transient is over once it turns down.
    falling: u8,
    /// Consecutive frames the low end has stayed lifted. A kick rings for
    /// several frames; a jitter in the noise floor lasts one, and confirming
    /// on that costs the next real drop, which then falls inside MIN_HOLD.
    bass_frames: u8,
}

/// Ignore onsets for this long after a switch.
const MIN_HOLD: f32 = 9.;
/// Switch regardless once a scene has been up this long: ambient tracks may
/// never produce a peak, and a scene that never changes is not "alternating".
const MAX_HOLD: f32 = 45.;
/// Flux samples kept for the running statistics (~4s at 60fps).
const FLUX_HIST: usize = 240;
/// Below this the spectrum is not moving enough for a peak to mean anything.
const FLUX_FLOOR: f32 = 0.012;
/// How many standard deviations above the recent mean arms the switch.
const FLUX_SIGMAS: f32 = 2.2;
/// ...but also at least this many times the mean. In a steady passage the
/// spread collapses, so a sigma test alone arms on ripples in the noise — and
/// a false arm costs the next real drop, which then falls inside MIN_HOLD.
const FLUX_RATIO: f32 = 2.5;
/// A flux peak this far out is a hit on its own, no low-end confirmation
/// needed — covers heavily compressed masters, where nothing gets louder.
const FLUX_SIGMAS_HARD: f32 = 3.6;
/// ...and, as with arming, it must also be this many times the mean. Without
/// the floor the sigma test passes on nothing at all in a steady passage,
/// where the spread collapses toward zero.
const FLUX_RATIO_HARD: f32 = 4.5;
/// The hit has to bring more low end than the run-up, by this factor.
const BASS_JUMP: f32 = 1.18;
/// How much the low end dominates the detection function.
const BASS_WEIGHT: f32 = 0.7;
/// Frames the low end must stay lifted before a hit counts as landed.
const BASS_CONFIRM_FRAMES: u8 = 3;
/// Give up on an armed onset that never lands, and wait for the next one.
const ARM_WINDOW: f32 = 1.4;

impl OnsetSwitcher {
    pub fn new() -> Self {
        Self {
            hist: VecDeque::with_capacity(FLUX_HIST),
            bass_hist: VecDeque::with_capacity(FLUX_HIST),
            since: 0.,
            armed: None,
        }
    }

    /// Feed one frame; returns true when the scene should change now.
    ///
    /// Two stages on purpose. Flux peaks on the *run-up* — a riser, a snare
    /// roll, a filter sweep all climb before the thing they lead into — so
    /// firing on the threshold crossing lands early, ahead of the beat.
    /// Crossing only arms the switch; it commits when the transient turns
    /// over, confirmed by the low end jumping. Low end is the tell twice over:
    /// it dominates the detection function (see [`Onset::strength`]) and it
    /// confirms the commit, because a riser is a narrow sweep up through the
    /// mids while the thing it leads into brings the kick and bass back.
    /// `energy` gates out silence, where flux is just noise around zero.
    pub fn observe(&mut self, onset: Onset, dt: f32) -> bool {
        let strength = onset.strength();
        self.since += dt;
        if self.hist.len() == FLUX_HIST {
            self.hist.pop_front();
            self.bass_hist.pop_front();
        }
        self.hist.push_back(strength);
        self.bass_hist.push_back(onset.bass);

        if self.since >= MAX_HOLD {
            self.reset();
            return true;
        }
        // Silence still ages the scene toward MAX_HOLD, but must not trigger:
        // the statistics of near-zero flux make any blip look significant.
        if self.since < MIN_HOLD || onset.energy < 0.02 || self.hist.len() < 30 {
            self.armed = None;
            return false;
        }

        let n = self.hist.len() as f32;
        let mean = self.hist.iter().sum::<f32>() / n;
        let sd = (self.hist.iter().map(|f| (f - mean).powi(2)).sum::<f32>() / n).sqrt();
        let bass_mean = self.bass_hist.iter().sum::<f32>() / n;

        match &mut self.armed {
            None => {
                let arm_at = (mean + FLUX_SIGMAS * sd).max(mean * FLUX_RATIO);
                if strength > arm_at && strength > FLUX_FLOOR {
                    self.armed = Some(Armed {
                        waited: 0.,
                        peak_flux: strength,
                        falling: 0,
                        bass_frames: 0,
                    });
                }
                false
            }
            Some(a) => {
                a.waited += dt;
                if strength >= a.peak_flux {
                    a.peak_flux = strength;
                    a.falling = 0;
                } else {
                    a.falling = a.falling.saturating_add(1);
                }
                if onset.bass > bass_mean * BASS_JUMP {
                    a.bass_frames = a.bass_frames.saturating_add(1);
                } else {
                    a.bass_frames = 0;
                }
                let hard = (mean + FLUX_SIGMAS_HARD * sd).max(mean * FLUX_RATIO_HARD);
                let landed = a.bass_frames >= BASS_CONFIRM_FRAMES || a.peak_flux > hard;
                if landed && a.falling >= 1 {
                    self.reset();
                    return true;
                }
                if a.waited >= ARM_WINDOW {
                    // Never resolved into a hit. Drop it rather than switch on
                    // a moment nobody heard; MAX_HOLD is the safety net.
                    self.armed = None;
                }
                false
            }
        }
    }

    fn reset(&mut self) {
        self.since = 0.;
        self.armed = None;
    }
}

/// Rolling analysis state. Lives in the view, ticked once per frame.
pub struct Visualizer {
    tap: Arc<SpectrumTap>,
    /// Scratch buffers, kept allocated across frames.
    samples: Vec<f32>,
    raw: Vec<f32>,
    /// Smoothed band levels actually drawn.
    bands: Vec<f32>,
    /// Terrain history, newest at the front.
    history: VecDeque<Vec<f32>>,
    /// Sample counter from the last tick — unchanged means no audio flowed, so
    /// the scene decays instead of freezing on the last frame.
    last_head: u64,
    last_tick: Instant,
    row_accum: f32,
    /// Accumulated rotation, advanced by elapsed time so the spin rate does not
    /// depend on frame rate.
    spin: f32,
    /// Previous frame's raw bands, for the spectral flux Auto switches on.
    prev_raw: Vec<f32>,
    /// Scene on screen, the one before it (excluded from the next random pick
    /// so Auto cannot ping-pong between two), and the one fading out behind.
    scene: Scene,
    prev_scene: Option<Scene>,
    fading: Option<Scene>,
    fade_left: f32,
    switcher: OnsetSwitcher,
    /// Retro scene: shapes persist across frames, so their randomness has to
    /// live here rather than being redrawn every paint.
    shapes: Vec<Shape>,
    rng: u32,
    /// Beat flash, driven by flux and decaying — what makes the background
    /// react rather than just glow.
    flash: f32,
    /// Unit icosphere for the Orb scene, built once and shared with each
    /// frame's paint callback.
    orb: Arc<OrbMesh>,
}

/// One mesh instance in the Retro scene.
#[derive(Clone, Copy)]
struct Shape {
    kind: MeshKind,
    /// Position in world units; z is depth toward the camera.
    x: f32,
    y: f32,
    z: f32,
    /// Rotation about each axis, and how fast it turns — three axes so the
    /// solids tumble rather than spin flat.
    rot: [f32; 3],
    rot_rate: [f32; 3],
    /// Hue offset from the accent, so the field is a family of colours rather
    /// than one flat tint.
    hue: f32,
    /// Which band drives this mesh's size.
    band: usize,
    /// Base half-extent before the band scales it.
    scale: f32,
    /// Seeds this instance's vertex deformation. Kept here (not re-rolled per
    /// frame) so a given mesh keeps its own lumpy shape while it flies.
    seed: u32,
}

/// The solids the scene picks from. Small, recognisable, and cheap to draw —
/// a handful of faces each, which matters when a couple of dozen are on
/// screen and every face is sorted and stroked on the CPU.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MeshKind {
    Tetra,
    Cube,
    Octa,
}

const TETRA_V: [[f32; 3]; 4] = [[1., 1., 1.], [1., -1., -1.], [-1., 1., -1.], [-1., -1., 1.]];
const TETRA_F: [&[usize]; 4] = [&[0, 1, 2], &[0, 3, 1], &[0, 2, 3], &[1, 3, 2]];

const CUBE_V: [[f32; 3]; 8] = [
    [-1., -1., -1.],
    [1., -1., -1.],
    [1., 1., -1.],
    [-1., 1., -1.],
    [-1., -1., 1.],
    [1., -1., 1.],
    [1., 1., 1.],
    [-1., 1., 1.],
];
const CUBE_F: [&[usize]; 6] = [
    &[0, 3, 2, 1],
    &[4, 5, 6, 7],
    &[0, 1, 5, 4],
    &[2, 3, 7, 6],
    &[1, 2, 6, 5],
    &[0, 4, 7, 3],
];

const OCTA_V: [[f32; 3]; 6] = [
    [1., 0., 0.],
    [-1., 0., 0.],
    [0., 1., 0.],
    [0., -1., 0.],
    [0., 0., 1.],
    [0., 0., -1.],
];
const OCTA_F: [&[usize]; 8] = [
    &[0, 2, 4],
    &[2, 1, 4],
    &[1, 3, 4],
    &[3, 0, 4],
    &[2, 0, 5],
    &[1, 2, 5],
    &[3, 1, 5],
    &[0, 3, 5],
];

impl MeshKind {
    fn geometry(self) -> (&'static [[f32; 3]], &'static [&'static [usize]]) {
        match self {
            Self::Tetra => (&TETRA_V, &TETRA_F),
            Self::Cube => (&CUBE_V, &CUBE_F),
            Self::Octa => (&OCTA_V, &OCTA_F),
        }
    }
}

/// Screen grid the projected vertices snap to. The PlayStation had no
/// sub-pixel precision in its rasteriser, so geometry visibly jittered as it
/// moved; quantising here is what produces that wobble.
const SNAP: f32 = 3.5;
/// How far vertices are pushed off the base solid, as a fraction of its size.
const DEFORM: f32 = 0.38;

/// Deterministic per-vertex noise in -1..1 from an instance seed and a vertex
/// index — a hash, not an RNG, so the same vertex deforms the same way on
/// every frame without storing the mesh.
fn vertex_noise(seed: u32, i: u32) -> f32 {
    let mut h = seed ^ i.wrapping_mul(0x9e37_79b9);
    h ^= h >> 15;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    (h >> 8) as f32 / 8_388_608. - 1.
}

/// Subdivisions of the base icosahedron for the Orb. Each level quadruples
/// the face count; 3 is dense enough to read as a sphere while keeping the
/// whole wireframe inside one tessellated path per frame.
const ORB_DETAIL: usize = 3;

/// Unit icosphere: vertices on the unit sphere plus the unique edges between
/// them. Built once and shared, since only the displacement changes per frame.
pub struct OrbMesh {
    verts: Vec<[f32; 3]>,
    edges: Vec<(u32, u32)>,
}

impl OrbMesh {
    fn new(detail: usize) -> Self {
        // Icosahedron: 12 vertices, 20 faces.
        let t = (1. + 5f32.sqrt()) / 2.;
        let mut verts: Vec<[f32; 3]> = vec![
            [-1., t, 0.],
            [1., t, 0.],
            [-1., -t, 0.],
            [1., -t, 0.],
            [0., -1., t],
            [0., 1., t],
            [0., -1., -t],
            [0., 1., -t],
            [t, 0., -1.],
            [t, 0., 1.],
            [-t, 0., -1.],
            [-t, 0., 1.],
        ];
        for v in &mut verts {
            let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
            *v = [v[0] / l, v[1] / l, v[2] / l];
        }
        let mut faces: Vec<[u32; 3]> = vec![
            [0, 11, 5],
            [0, 5, 1],
            [0, 1, 7],
            [0, 7, 10],
            [0, 10, 11],
            [1, 5, 9],
            [5, 11, 4],
            [11, 10, 2],
            [10, 7, 6],
            [7, 1, 8],
            [3, 9, 4],
            [3, 4, 2],
            [3, 2, 6],
            [3, 6, 8],
            [3, 8, 9],
            [4, 9, 5],
            [2, 4, 11],
            [6, 2, 10],
            [8, 6, 7],
            [9, 8, 1],
        ];

        // Subdivide: split every edge at its midpoint, pushed back out to the
        // unit sphere. The cache keeps split edges shared between the two
        // faces that own them, so the mesh stays welded.
        for _ in 0..detail {
            let mut cache: std::collections::HashMap<(u32, u32), u32> = Default::default();
            let mut next = Vec::with_capacity(faces.len() * 4);
            for f in &faces {
                let mut mid = [0u32; 3];
                for (k, (a, b)) in [(f[0], f[1]), (f[1], f[2]), (f[2], f[0])]
                    .into_iter()
                    .enumerate()
                {
                    let key = (a.min(b), a.max(b));
                    mid[k] = *cache.entry(key).or_insert_with(|| {
                        let (va, vb) = (verts[a as usize], verts[b as usize]);
                        let m = [
                            (va[0] + vb[0]) / 2.,
                            (va[1] + vb[1]) / 2.,
                            (va[2] + vb[2]) / 2.,
                        ];
                        let l = (m[0] * m[0] + m[1] * m[1] + m[2] * m[2]).sqrt();
                        verts.push([m[0] / l, m[1] / l, m[2] / l]);
                        verts.len() as u32 - 1
                    });
                }
                next.push([f[0], mid[0], mid[2]]);
                next.push([f[1], mid[1], mid[0]]);
                next.push([f[2], mid[2], mid[1]]);
                next.push([mid[0], mid[1], mid[2]]);
            }
            faces = next;
        }

        // Unique edges — drawing per face would stroke every shared edge twice.
        let mut seen = std::collections::HashSet::new();
        let mut edges = Vec::new();
        for f in &faces {
            for (a, b) in [(f[0], f[1]), (f[1], f[2]), (f[2], f[0])] {
                if seen.insert((a.min(b), a.max(b))) {
                    edges.push((a, b));
                }
            }
        }
        Self { verts, edges }
    }
}

/// Value noise in 3D, smoothed, roughly -1..1. Stands in for the simplex noise
/// the look is usually built on: the displacement only needs to be continuous
/// and non-repeating at this scale, and this is a fraction of the code.
fn noise3(x: f32, y: f32, z: f32) -> f32 {
    fn hash(xi: i32, yi: i32, zi: i32) -> f32 {
        let mut h = (xi.wrapping_mul(374_761_393)
            ^ yi.wrapping_mul(668_265_263)
            ^ zi.wrapping_mul(2_147_483_647)) as u32;
        h ^= h >> 13;
        h = h.wrapping_mul(1_274_126_177);
        h ^= h >> 16;
        (h >> 8) as f32 / 8_388_608. - 1.
    }
    let (xi, yi, zi) = (x.floor(), y.floor(), z.floor());
    let (fx, fy, fz) = (x - xi, y - yi, z - zi);
    // Smoothstep so the lattice does not show as creases.
    let (sx, sy, sz) = (
        fx * fx * (3. - 2. * fx),
        fy * fy * (3. - 2. * fy),
        fz * fz * (3. - 2. * fz),
    );
    let (xi, yi, zi) = (xi as i32, yi as i32, zi as i32);
    let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
    let c = |dx, dy, dz| hash(xi + dx, yi + dy, zi + dz);
    let x00 = lerp(c(0, 0, 0), c(1, 0, 0), sx);
    let x10 = lerp(c(0, 1, 0), c(1, 1, 0), sx);
    let x01 = lerp(c(0, 0, 1), c(1, 0, 1), sx);
    let x11 = lerp(c(0, 1, 1), c(1, 1, 1), sx);
    lerp(lerp(x00, x10, sy), lerp(x01, x11, sy), sz)
}

/// Shapes in flight at once.
const SHAPES: usize = 26;
/// Depth a recycled shape re-enters at.
const SHAPE_FAR: f32 = 7.5;
/// Depth at which a shape has passed the camera and is recycled.
const SHAPE_NEAR: f32 = 0.6;

impl Visualizer {
    pub fn new(tap: Arc<SpectrumTap>) -> Self {
        let mut viz = Self {
            tap,
            samples: vec![0.; FFT_SIZE],
            raw: vec![0.; BANDS],
            bands: vec![0.; BANDS],
            history: VecDeque::from(vec![vec![0.; BANDS]; ROWS]),
            last_head: 0,
            last_tick: Instant::now(),
            row_accum: 0.,
            spin: 0.,
            prev_raw: vec![0.; BANDS],
            scene: Scene::Terrain,
            prev_scene: None,
            fading: None,
            fade_left: 0.,
            switcher: OnsetSwitcher::new(),
            shapes: Vec::new(),
            rng: 0x2545_f491,
            flash: 0.,
            orb: Arc::new(OrbMesh::new(ORB_DETAIL)),
        };
        // Spread the initial field through the depth range so the scene is
        // already populated the first time it is shown.
        for i in 0..SHAPES {
            let mut shape = viz.spawn_shape();
            shape.z = SHAPE_NEAR + (SHAPE_FAR - SHAPE_NEAR) * (i as f32 / SHAPES as f32);
            viz.shapes.push(shape);
        }
        viz
    }

    /// Cheap deterministic randomness — a visualizer does not need a real RNG,
    /// and keeping it in-struct means no global state and reproducible tests.
    fn rand(&mut self) -> f32 {
        self.rng = self.rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.rng >> 8) as f32 / 16_777_216.
    }

    /// Pick the next Auto scene at random rather than walking a fixed
    /// rotation, excluding the current scene and the one before it — a cycle
    /// becomes predictable after one pass, and an unconstrained random pick
    /// repeats or ping-pongs often enough to look like one.
    fn pick_scene(&mut self) -> Scene {
        let mut pool: Vec<Scene> = Scene::ALL
            .into_iter()
            .filter(|s| *s != self.scene && Some(*s) != self.prev_scene)
            .collect();
        if pool.is_empty() {
            pool = Scene::ALL
                .into_iter()
                .filter(|s| *s != self.scene)
                .collect();
        }
        let i = ((self.rand() * pool.len() as f32) as usize).min(pool.len() - 1);
        pool[i]
    }

    fn spawn_shape(&mut self) -> Shape {
        let (a, b, c, d, e, f, g) = (
            self.rand(),
            self.rand(),
            self.rand(),
            self.rand(),
            self.rand(),
            self.rand(),
            self.rand(),
        );
        Shape {
            kind: match (c * 3.) as u8 {
                0 => MeshKind::Tetra,
                1 => MeshKind::Cube,
                _ => MeshKind::Octa,
            },
            x: (a - 0.5) * 3.4,
            y: (b - 0.5) * 2.4,
            z: SHAPE_FAR,
            rot: [
                d * std::f32::consts::TAU,
                e * std::f32::consts::TAU,
                f * std::f32::consts::TAU,
            ],
            rot_rate: [(d - 0.5) * 1.6, (e - 0.5) * 1.6, (g - 0.5) * 1.2],
            hue: (f - 0.5) * 0.22,
            band: (a * BANDS as f32) as usize % BANDS,
            scale: 0.13 + 0.2 * b,
            seed: self.rng,
        }
    }

    /// Read the newest samples, run the FFT, and advance the animation clocks.
    /// Call once per rendered frame, with the mode currently selected.
    pub fn tick(&mut self, mode: VisualizerMode) {
        let now = Instant::now();
        // Clamped: a frame delayed by a stall (or the overlay being reopened
        // after minutes) must not teleport the scene.
        let dt = (now - self.last_tick).as_secs_f32().min(0.1);
        self.last_tick = now;
        self.advance(mode, dt);
    }

    /// One step of `dt` seconds. Split out from `tick` so the decay, the scene
    /// clocks and the switching can be driven at a known rate in tests.
    fn advance(&mut self, mode: VisualizerMode, dt: f32) {
        self.spin += dt;

        let head = self.tap.snapshot(&mut self.samples);
        if head == self.last_head {
            // Paused or starved: fall to silence rather than hold the last
            // frame, which would look frozen rather than quiet.
            self.raw.iter_mut().for_each(|b| *b = 0.);
        } else {
            spectrum::analyze(&self.samples, &mut self.raw, self.tap.sample_rate());
        }
        self.last_head = head;

        // Fast attack, slow release: peaks land on the beat, decay reads as
        // the note ringing out instead of flickering.
        let attack = 1. - (-dt * 26.).exp();
        let release = 1. - (-dt * 7.).exp();
        for (out, &target) in self.bands.iter_mut().zip(self.raw.iter()) {
            let k = if target > *out { attack } else { release };
            *out += (target - *out) * k;
        }

        // Spectral flux: only rises count, so a note starting registers and a
        // note ending does not. Taken from the raw bands — the smoothed ones
        // have had exactly these transients filed off.
        let flux = self
            .raw
            .iter()
            .zip(self.prev_raw.iter())
            .map(|(now, before)| (now - before).max(0.))
            .sum::<f32>()
            / BANDS as f32;
        // Same measure over the bottom of the spectrum only: this is what
        // makes the switch land on the kick rather than on any busy moment.
        let low = BANDS / 4;
        let bass_flux = self
            .raw
            .iter()
            .zip(self.prev_raw.iter())
            .take(low)
            .map(|(now, before)| (now - before).max(0.))
            .sum::<f32>()
            / low as f32;
        self.prev_raw.copy_from_slice(&self.raw);

        // Beat flash: jumps with flux, bleeds away. The background rides this,
        // which is what makes it feel struck rather than merely animated.
        self.flash = (self.flash - dt * 2.6).max(0.).max((flux * 9.).min(1.));

        // Shapes fly at the camera, faster when the track is busy, and are
        // recycled from the back once they pass it.
        let speed = 0.55 + 1.9 * self.energy();
        for i in 0..self.shapes.len() {
            self.shapes[i].z -= speed * dt;
            for axis in 0..3 {
                self.shapes[i].rot[axis] += self.shapes[i].rot_rate[axis] * dt;
            }
            if self.shapes[i].z <= SHAPE_NEAR {
                self.shapes[i] = self.spawn_shape();
            }
        }

        self.fade_left = (self.fade_left - dt).max(0.);
        if self.fade_left == 0. {
            self.fading = None;
        }

        let want = match pinned_scene(mode) {
            Some(pinned) => pinned,
            None if mode == VisualizerMode::Auto => {
                let onset = Onset {
                    flux,
                    bass_flux,
                    energy: self.energy(),
                    bass: self.bass(),
                };
                if self.switcher.observe(onset, dt) {
                    self.pick_scene()
                } else {
                    self.scene
                }
            }
            // Off: hold the current scene so reopening does not restart it.
            None => self.scene,
        };
        if want != self.scene {
            self.prev_scene = Some(self.scene);
            self.fading = Some(self.scene);
            self.fade_left = FADE;
            self.scene = want;
        }

        self.row_accum += dt;
        while self.row_accum >= ROW_INTERVAL {
            self.row_accum -= ROW_INTERVAL;
            self.history.pop_back();
            self.history.push_front(self.bands.clone());
        }
    }

    /// Mean level across the bands, used to drive scene-wide reactions.
    fn energy(&self) -> f32 {
        self.bands.iter().sum::<f32>() / BANDS as f32
    }

    /// Mean level of the bottom quarter of the spectrum (~30-170Hz): kick and
    /// bass. Used to tell a drop from the riser leading into it.
    fn bass(&self) -> f32 {
        let n = BANDS / 4;
        self.bands.iter().take(n).sum::<f32>() / n as f32
    }

    /// The scene as a full-size element. `accent` tints the geometry; pass the
    /// cover-derived accent so the visualizer matches the rest of the overlay.
    pub fn render(&self, accent: Hsla) -> gpui::AnyElement {
        use gpui::{IntoElement as _, Styled as _};

        // The paint callback runs after this borrow ends, so hand it owned
        // copies of everything it needs.
        let bands = self.bands.clone();
        let history: Vec<Vec<f32>> = self.history.iter().cloned().collect();
        let spin = self.spin;
        let energy = self.energy();
        let scene = self.scene;
        let flash = self.flash;
        let bass = self.bass();
        let shapes = self.shapes.clone();
        let orb = self.orb.clone();
        // Treble drives the noise the way bass drives the size, so the orb
        // spikes on hats and swells on kicks.
        let treble = {
            let hi = BANDS / 2;
            self.bands.iter().skip(hi).sum::<f32>() / (BANDS - hi) as f32
        };
        // Incoming scene rises fast, outgoing one falls over the whole fade:
        // the switch is meant to land on the beat, not to dissolve through it.
        let t = 1. - self.fade_left / FADE;
        let outgoing = self.fading.map(|s| (s, 1. - t));
        let incoming = (t * 2.5).min(1.);

        canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                let mut draw = |scene: Scene, alpha: f32| match scene {
                    Scene::Terrain => paint_terrain(bounds, &history, accent, alpha, window),
                    Scene::Tunnel => {
                        paint_tunnel(bounds, &bands, spin, energy, accent, alpha, window)
                    }
                    Scene::Sphere => {
                        paint_sphere(bounds, &bands, spin, energy, accent, alpha, window)
                    }
                    Scene::Retro => {
                        paint_retro(bounds, &bands, &shapes, flash, bass, accent, alpha, window)
                    }
                    Scene::Orb => {
                        paint_orb(bounds, &orb, spin, bass, treble, accent, alpha, window)
                    }
                };
                if let Some((prev, alpha)) = outgoing {
                    draw(prev, alpha);
                }
                draw(scene, incoming);
            },
        )
        .size_full()
        .into_any_element()
    }
}

/// A point in world space: x right, y up, z into the screen.
#[derive(Clone, Copy)]
struct P3 {
    x: f32,
    y: f32,
    z: f32,
}

/// Perspective divide. Returns `None` for anything at or behind the near
/// plane, which would otherwise project to a mirrored point.
fn project(p: P3, bounds: Bounds<Pixels>, focal: f32) -> Option<Point<Pixels>> {
    if p.z <= 0.05 {
        return None;
    }
    let cx = f32::from(bounds.origin.x) + f32::from(bounds.size.width) / 2.;
    let cy = f32::from(bounds.origin.y) + f32::from(bounds.size.height) / 2.;
    let k = focal / p.z;
    Some(point(px(cx + p.x * k), px(cy - p.y * k)))
}

/// Focal length scaled to the viewport so the scene fills wide and narrow
/// windows the same way. `scale` is the per-scene zoom.
fn focal_of(bounds: Bounds<Pixels>, scale: f32) -> f32 {
    f32::from(bounds.size.width).min(f32::from(bounds.size.height) * 1.6) * scale
}

/// Band level at a fractional position across the spectrum, linearly
/// interpolated. Sampling the nearest band instead makes any smooth sweep
/// across the bands come out as a staircase.
fn band_at(bands: &[f32], t: f32) -> f32 {
    let x = t.clamp(0., 1.) * (bands.len() - 1) as f32;
    let i = x.floor() as usize;
    let f = x - i as f32;
    let a = bands[i];
    let b = bands[(i + 1).min(bands.len() - 1)];
    a + (b - a) * f
}

/// Accent shifted and faded by depth: far geometry sits back into the
/// background instead of competing with the controls on top of it.
fn depth_color(accent: Hsla, depth: f32, alpha: f32) -> Hsla {
    let t = depth.clamp(0., 1.);
    Hsla {
        h: (accent.h + 0.06 * t).fract(),
        s: (accent.s * (1. - 0.35 * t)).clamp(0., 1.),
        l: (accent.l * (1. - 0.55 * t) + 0.05).clamp(0., 1.),
        a: alpha.clamp(0., 1.),
    }
}

/// Scrolling spectrum landscape: frequency across x, time receding into z,
/// level as height. Rows are drawn far to near, each filled below its ridge so
/// it hides the rows behind it.
fn paint_terrain(
    bounds: Bounds<Pixels>,
    history: &[Vec<f32>],
    accent: Hsla,
    alpha: f32,
    window: &mut gpui::Window,
) {
    let focal = focal_of(bounds, 0.55);
    // World units. The grid is wider than it is deep so the near row spans the
    // viewport without the far rows collapsing to a point too quickly.
    let half_w = 1.9;
    let base_y = -0.5;
    let height = 1.05;
    let z_near = 1.7;
    let z_step = 0.28;
    // Rows are filled from their ridge down to well below the frame: an
    // exactly-baseline fill leaves slits of background between rows, which
    // breaks the illusion that they are a continuous surface.
    let floor_y = -12.;

    for (r, row) in history.iter().enumerate().rev() {
        let z = z_near + r as f32 * z_step;
        let depth = r as f32 / history.len().max(1) as f32;
        let x_at = |b: usize| (b as f32 / (BANDS - 1) as f32 - 0.5) * 2. * half_w;

        let mut ridge = Vec::with_capacity(BANDS);
        for (b, &v) in row.iter().enumerate() {
            let p = P3 {
                x: x_at(b),
                y: base_y + v * height,
                z,
            };
            match project(p, bounds, focal) {
                Some(pt) => ridge.push(pt),
                None => return,
            }
        }
        let (Some(floor_l), Some(floor_r)) = (
            project(
                P3 {
                    x: x_at(0),
                    y: floor_y,
                    z,
                },
                bounds,
                focal,
            ),
            project(
                P3 {
                    x: x_at(BANDS - 1),
                    y: floor_y,
                    z,
                },
                bounds,
                focal,
            ),
        ) else {
            return;
        };

        // Body: dark and opaque, so it hides the rows behind it and leaves the
        // lit ridge on top as the thing you actually read.
        let mut body = PathBuilder::fill();
        body.move_to(floor_l);
        for pt in &ridge {
            body.line_to(*pt);
        }
        body.line_to(floor_r);
        body.close();
        if let Ok(path) = body.build() {
            let mut c = depth_color(accent, depth, alpha);
            c.l *= 0.14;
            c.s *= 0.6;
            window.paint_path(path, c);
        }

        let mut line = PathBuilder::stroke(px(1.8 - 0.9 * depth));
        line.move_to(ridge[0]);
        for pt in &ridge[1..] {
            line.line_to(*pt);
        }
        if let Ok(path) = line.build() {
            window.paint_path(
                path,
                depth_color(accent, depth, (0.95 - 0.6 * depth) * alpha),
            );
        }
    }
}

/// Rings receding to a vanishing point, each ring's radius modulated per angle
/// by the spectrum. The whole tunnel drifts toward the viewer, with rings
/// recycling from the back so the flight never restarts visibly.
fn paint_tunnel(
    bounds: Bounds<Pixels>,
    bands: &[f32],
    spin: f32,
    energy: f32,
    accent: Hsla,
    alpha: f32,
    window: &mut gpui::Window,
) {
    const RINGS: usize = 18;
    const SEGMENTS: usize = 96;
    let focal = focal_of(bounds, 0.7);
    let z_near = 1.05;
    let z_step = 0.5;
    // Continuous forward drift; `fract` recycles the nearest ring to the back.
    let drift = (spin * 0.75).fract();

    for i in (0..RINGS).rev() {
        let z = z_near + (i as f32 + drift) * z_step;
        let depth = i as f32 / RINGS as f32;
        // Rings turn as a whole, with only a slight lean per ring: twisting
        // hard with depth makes a single loud band spiral across the rings and
        // read as a stray diagonal streak rather than as a ridge in the tube.
        let twist = spin * 0.3 + z * 0.06;

        let mut pb = PathBuilder::stroke(px(2.4 - 1.6 * depth));
        let mut first = None;
        for seg in 0..SEGMENTS {
            let a = seg as f32 / SEGMENTS as f32 * std::f32::consts::TAU;
            // Mirror the spectrum across the ring so the shape is symmetric
            // instead of seamed where the band list wraps.
            let t = (a / std::f32::consts::TAU * 2. - 1.).abs();
            // Smoothed across neighbouring bands: a lone loud band would
            // otherwise spike every ring at the same angle, and the aligned
            // tips read as a stray radial line through the tunnel.
            let level =
                (band_at(bands, t - 0.04) + 2. * band_at(bands, t) + band_at(bands, t + 0.04)) / 4.;
            let radius = 0.5 * (1. + 0.5 * level);
            let (sin, cos) = (a + twist).sin_cos();
            let Some(pt) = project(
                P3 {
                    x: cos * radius,
                    y: sin * radius,
                    z,
                },
                bounds,
                focal,
            ) else {
                continue;
            };
            match first {
                None => {
                    pb.move_to(pt);
                    first = Some(pt);
                }
                Some(_) => pb.line_to(pt),
            }
        }
        if first.is_none() {
            continue;
        }
        // Closing the subpath makes lyon join the last segment to the first.
        // Repeating the start point instead leaves a zero-length segment whose
        // miter shoots off outward — with every ring seamed at the same angle,
        // those spurs line up into a stray radial streak.
        pb.close();
        if let Ok(path) = pb.build() {
            let a = (1. - depth).powf(1.3) * (0.5 + 0.5 * energy) * alpha;
            window.paint_path(path, depth_color(accent, depth, a));
        }
    }
}

/// Rotating point cloud on a Fibonacci sphere, each point pushed out along its
/// normal by the band it belongs to. Points are quads sized by depth, which is
/// both cheaper than paths and reads as a proper 3D cloud.
fn paint_sphere(
    bounds: Bounds<Pixels>,
    bands: &[f32],
    spin: f32,
    energy: f32,
    accent: Hsla,
    alpha: f32,
    window: &mut gpui::Window,
) {
    let focal = focal_of(bounds, 0.9);
    let z_cam = 2.6;
    let golden = std::f32::consts::PI * (3. - 5f32.sqrt());
    let (yaw, pitch) = (spin * 0.55, (spin * 0.23).sin() * 0.45);
    let (sy, cy) = yaw.sin_cos();
    let (sp, cp) = pitch.sin_cos();

    // Painter's algorithm: collect, sort back to front, then draw.
    let mut pts: Vec<(f32, Point<Pixels>, f32)> = Vec::with_capacity(POINTS);
    for i in 0..POINTS {
        // Fibonacci sphere: even coverage without pole clustering.
        let y = 1. - (i as f32 / (POINTS - 1) as f32) * 2.;
        let r = (1. - y * y).max(0.).sqrt();
        let theta = golden * i as f32;
        let (x, z) = (theta.cos() * r, theta.sin() * r);

        // Bass at the equator, treble toward the poles: mapping bands to the
        // point index instead puts the loud low end on one pole and the cloud
        // reads as lopsided rather than as a sphere.
        let band = band_at(bands, y.abs());
        let radius = 0.75 * (1. + 0.55 * band);
        let (px_, py_, pz_) = (x * radius, y * radius, z * radius);

        // Yaw about y, then pitch about x.
        let (rx, rz) = (px_ * cy + pz_ * sy, -px_ * sy + pz_ * cy);
        let (ry, rz) = (py_ * cp - rz * sp, py_ * sp + rz * cp);

        let world = P3 {
            x: rx,
            y: ry,
            z: z_cam + rz,
        };
        if let Some(pt) = project(world, bounds, focal) {
            pts.push((world.z, pt, band));
        }
    }
    pts.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let (z_min, z_max) = (z_cam - 1.3, z_cam + 1.3);
    for (z, pt, band) in pts {
        let depth = ((z - z_min) / (z_max - z_min)).clamp(0., 1.);
        // Loud points are both bigger and brighter, so the cloud pulses rather
        // than just shifting colour.
        let size = (focal / z) * (0.014 + 0.035 * band);
        let a = (1. - 0.85 * depth) * (0.3 + 0.7 * (band + energy * 0.5).min(1.)) * alpha;
        let half = px(size / 2.);
        let quad = Bounds {
            origin: point(pt.x - half, pt.y - half),
            size: gpui::size(px(size), px(size)),
        };
        window.paint_quad(fill(quad, depth_color(accent, depth, a)).corner_radii(half));
    }
}

/// Randomly generated wireframe shapes flying at the camera over a background
/// that reacts to the track — the 2000s media-player look.
///
/// The shapes are spawned and moved in `Visualizer::advance` (they have to
/// persist between frames to fly anywhere); this only draws the state.
#[allow(clippy::too_many_arguments)]
fn paint_retro(
    bounds: Bounds<Pixels>,
    bands: &[f32],
    shapes: &[Shape],
    flash: f32,
    bass: f32,
    accent: Hsla,
    alpha: f32,
    window: &mut gpui::Window,
) {
    let focal = focal_of(bounds, 0.8);

    // Reactive background: a wash whose lightness and saturation ride the low
    // end, kicked brighter by the flash on every hit.
    let lift = 0.35 * bass + 0.65 * flash;
    let top = Hsla {
        h: (accent.h + 0.08 + 0.06 * flash).rem_euclid(1.),
        s: (0.4 + 0.35 * lift).clamp(0., 1.),
        l: (0.05 + 0.13 * lift).clamp(0., 1.),
        a: alpha,
    };
    let bottom = Hsla {
        h: (accent.h - 0.05 * flash).rem_euclid(1.),
        s: (0.5 + 0.3 * lift).clamp(0., 1.),
        l: (0.03 + 0.20 * lift).clamp(0., 1.),
        a: alpha,
    };
    window.paint_quad(fill(
        bounds,
        linear_gradient(
            160.,
            linear_color_stop(top, 0.),
            linear_color_stop(bottom, 1.),
        ),
    ));

    // Horizon beam: swells with the bass behind the shapes, so they read as
    // flying through something. Built as two mirrored halves because gpui's
    // linear_gradient takes exactly two stops — a single rect fading in but
    // never out leaves a hard edge across the middle of the screen.
    let h = f32::from(bounds.size.height);
    let beam_h = h * (0.10 + 0.24 * lift);
    let mid = f32::from(bounds.origin.y) + h * 0.52;
    let core = Hsla {
        l: (accent.l + 0.18).min(1.),
        a: (0.10 + 0.32 * lift) * alpha,
        ..accent
    };
    let edge = Hsla { a: 0., ..core };
    for (top_y, from, to) in [(mid - beam_h / 2., edge, core), (mid, core, edge)] {
        window.paint_quad(fill(
            Bounds {
                origin: point(bounds.origin.x, px(top_y)),
                size: gpui::size(bounds.size.width, px(beam_h / 2.)),
            },
            linear_gradient(180., linear_color_stop(from, 0.), linear_color_stop(to, 1.)),
        ));
    }

    // Meshes, far to near so the near ones sit on top.
    let mut order: Vec<&Shape> = shapes.iter().collect();
    order.sort_by(|a, b| b.z.partial_cmp(&a.z).unwrap_or(std::cmp::Ordering::Equal));

    for shape in order {
        let level = bands[shape.band];
        let size = shape.scale * (0.7 + 0.9 * level);
        // Meshes are opaque — flat-shaded hardware had no alpha to spare, and
        // fading them by distance instead of fogging them makes solids you can
        // see through. Alpha covers only the scene cross-fade and the dissolve
        // as a mesh reaches the camera; distance is carried by fog, and level
        // by brightness.
        let near_fade = ((shape.z - SHAPE_NEAR) / 0.8).clamp(0., 1.);
        let gain = near_fade * alpha;
        if gain <= 0.01 {
            continue;
        }

        let (verts, faces) = shape.kind.geometry();
        let [rx, ry, rz] = shape.rot;
        let (sx, cx_) = rx.sin_cos();
        let (sy, cy_) = ry.sin_cos();
        let (sz, cz_) = rz.sin_cos();
        // Deform, rotate about x then y then z, scale, then move into place.
        // The deformation is the point: PlayStation-era models were coarse and
        // lumpy, so a clean platonic solid reads as the wrong decade.
        let world: Vec<P3> = verts
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let d = |axis: u32| 1. + DEFORM * vertex_noise(shape.seed, i as u32 * 3 + axis);
                let (x, y, z) = (v[0] * size * d(0), v[1] * size * d(1), v[2] * size * d(2));
                let (y, z) = (y * cx_ - z * sx, y * sx + z * cx_);
                let (x, z) = (x * cy_ + z * sy, -x * sy + z * cy_);
                let (x, y) = (x * cz_ - y * sz, x * sz + y * cz_);
                P3 {
                    x: x + shape.x,
                    y: y + shape.y,
                    z: z + shape.z,
                }
            })
            .collect();

        let hue = (accent.h + shape.hue + 0.12 * flash).rem_euclid(1.);
        // Faces back to front within the mesh: flat-shaded faces are opaque,
        // and with no depth buffer, vertex order would show the far side of a
        // solid on top of the near one.
        let mut face_order: Vec<(f32, &&[usize])> = faces
            .iter()
            .map(|f| {
                let z = f.iter().map(|i| world[*i].z).sum::<f32>() / f.len() as f32;
                (z, f)
            })
            .collect();
        face_order.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        for (face_z, face) in face_order {
            let pts: Option<Vec<Point<Pixels>>> = face
                .iter()
                .map(|i| {
                    project(world[*i], bounds, focal).map(|p| {
                        // Vertex snapping — the wobble that dates the look.
                        point(
                            px((f32::from(p.x) / SNAP).round() * SNAP),
                            px((f32::from(p.y) / SNAP).round() * SNAP),
                        )
                    })
                })
                .collect();
            let Some(pts) = pts else { continue };
            if pts.len() < 3 {
                continue;
            }

            // Back-face cull from the winding of the projected polygon: a
            // negative signed area is a face pointing away, and drawing it
            // would just be overdraw behind an opaque front face.
            let area: f32 = (0..pts.len())
                .map(|i| {
                    let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
                    f32::from(a.x) * f32::from(b.y) - f32::from(b.x) * f32::from(a.y)
                })
                .sum::<f32>()
                / 2.;
            if area <= 0. {
                continue;
            }

            // Flat shading: one normal, one colour per face, no interpolation
            // across it — the hard facet edges are the era's signature.
            let (a, b, c) = (world[face[0]], world[face[1]], world[face[2]]);
            let (u, v) = (
                [b.x - a.x, b.y - a.y, b.z - a.z],
                [c.x - a.x, c.y - a.y, c.z - a.z],
            );
            let n = [
                u[1] * v[2] - u[2] * v[1],
                u[2] * v[0] - u[0] * v[2],
                u[0] * v[1] - u[1] * v[0],
            ];
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt().max(1e-6);
            // Key light over the viewer's shoulder.
            const LIGHT: [f32; 3] = [0.42, 0.63, -0.65];
            let diffuse = ((n[0] * LIGHT[0] + n[1] * LIGHT[1] + n[2] * LIGHT[2]) / len)
                .max(0.)
                .powf(0.8);

            // Depth fog toward the background, the era's other tell — and the
            // reason distant meshes do not clutter the frame.
            let fog = ((face_z - SHAPE_NEAR) / (SHAPE_FAR - SHAPE_NEAR)).clamp(0., 1.);
            let lit = 0.12 + 0.42 * diffuse + 0.2 * level + 0.12 * flash;

            let mut fill_path = PathBuilder::fill();
            fill_path.move_to(pts[0]);
            for pt in &pts[1..] {
                fill_path.line_to(*pt);
            }
            fill_path.close();
            if let Ok(path) = fill_path.build() {
                window.paint_path(
                    path,
                    Hsla {
                        h: hue,
                        s: ((accent.s + 0.1) * (1. - 0.55 * fog)).clamp(0., 1.),
                        l: (lit * (1. - 0.7 * fog)).clamp(0., 1.),
                        a: gain,
                    },
                );
            }
        }
    }
}

/// Wireframe icosphere that bass inflates and treble roughens: every vertex is
/// pushed along its own normal by 3D noise sampled at a slowly drifting
/// offset, so the surface boils rather than pulses uniformly.
///
/// The whole wireframe goes into a single path. Stroking each edge separately
/// would mean a thousand-odd tessellations per frame; as subpaths of one path
/// it is one.
#[allow(clippy::too_many_arguments)]
fn paint_orb(
    bounds: Bounds<Pixels>,
    orb: &OrbMesh,
    time: f32,
    bass: f32,
    treble: f32,
    accent: Hsla,
    alpha: f32,
    window: &mut gpui::Window,
) {
    let focal = focal_of(bounds, 0.95);
    let z_cam = 3.1;
    // Slow tumble on all three axes — no axis is ever still, so the sphere
    // never reads as a flat disc.
    let (rx, ry, rz) = (time * 0.06, time * 0.18, time * 0.30);
    let (sx, cx_) = rx.sin_cos();
    let (sy, cy_) = ry.sin_cos();
    let (sz, cz_) = rz.sin_cos();

    let radius = 0.58 * (1. + 0.42 * bass);
    let rough = 0.10 + 0.62 * treble;
    // Noise drifts at a different rate per axis so the surface never repeats.
    let (dx, dy, dz) = (time * 0.25, time * 0.37, time * 0.44);

    let screen: Vec<Option<Point<Pixels>>> = orb
        .verts
        .iter()
        .map(|v| {
            let n = noise3(v[0] * 1.9 + dx, v[1] * 1.9 + dy, v[2] * 1.9 + dz);
            let r = radius + n * rough * radius;
            let (x, y, z) = (v[0] * r, v[1] * r, v[2] * r);
            let (y, z) = (y * cx_ - z * sx, y * sx + z * cx_);
            let (x, z) = (x * cy_ + z * sy, -x * sy + z * cy_);
            let (x, y) = (x * cz_ - y * sz, x * sz + y * cz_);
            project(P3 { x, y, z: z + z_cam }, bounds, focal)
        })
        .collect();

    let mut pb = PathBuilder::stroke(px(1.15));
    let mut any = false;
    for (a, b) in &orb.edges {
        let (Some(pa), Some(pb_)) = (screen[*a as usize], screen[*b as usize]) else {
            continue;
        };
        pb.move_to(pa);
        pb.line_to(pb_);
        any = true;
    }
    if !any {
        return;
    }
    if let Ok(path) = pb.build() {
        window.paint_path(
            path,
            Hsla {
                h: accent.h,
                s: (accent.s * 0.55).clamp(0., 1.),
                l: (0.5 + 0.26 * bass).min(0.9),
                a: (0.5 + 0.45 * (bass + treble).min(1.)) * alpha,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_cycles_back_to_off() {
        // Walk the whole cycle rather than a fixed count, so adding a scene
        // does not silently make this assert nothing.
        let mut m = VisualizerMode::Off;
        let mut seen = vec![m];
        for _ in 0..16 {
            m = m.next();
            if m == VisualizerMode::Off {
                break;
            }
            assert!(
                !seen.contains(&m),
                "cycle repeats {m:?} before reaching Off"
            );
            seen.push(m);
        }
        assert_eq!(m, VisualizerMode::Off, "cycle never returned to Off");
        assert!(
            seen.contains(&VisualizerMode::Auto) && seen.contains(&VisualizerMode::Retro),
            "cycle skipped a mode: {seen:?}"
        );
        assert!(!VisualizerMode::Off.is_on());
        assert!(VisualizerMode::Tunnel.is_on());
    }

    /// Steady, unremarkable input: flux jitters around a low level, with a
    /// constant amount of low end.
    fn feed_calm(sw: &mut OnsetSwitcher, seconds: f32) -> bool {
        let mut fired = false;
        let mut i = 0;
        while (i as f32) * (1. / 60.) < seconds {
            let f = 0.004 + 0.0005 * ((i % 7) as f32);
            fired |= sw.observe(
                Onset {
                    flux: f,
                    bass_flux: f,
                    energy: 0.4,
                    bass: 0.2,
                },
                1. / 60.,
            );
            i += 1;
        }
        fired
    }

    /// A hit: flux spikes with the low end jumping, then turns over.
    fn feed_hit(sw: &mut OnsetSwitcher, flux: f32, bass_flux: f32) -> bool {
        let peak = Onset {
            flux,
            bass_flux,
            energy: 0.6,
            bass: 0.5,
        };
        let mut fired = sw.observe(peak, 1. / 60.);
        fired |= sw.observe(
            Onset {
                flux: flux * 0.4,
                bass_flux: bass_flux * 0.4,
                ..peak
            },
            1. / 60.,
        );
        fired
    }

    #[test]
    fn auto_switches_on_a_flux_spike_once_the_hold_is_over() {
        let mut sw = OnsetSwitcher::new();
        assert!(!feed_calm(&mut sw, MIN_HOLD + 1.), "fired while calm");
        assert!(feed_hit(&mut sw, 0.3, 0.3), "a drop should switch");
    }

    #[test]
    fn auto_ignores_spikes_during_the_hold() {
        let mut sw = OnsetSwitcher::new();
        feed_calm(&mut sw, MIN_HOLD - 2.);
        assert!(
            !feed_hit(&mut sw, 0.3, 0.3),
            "switched again inside the hold"
        );
    }

    #[test]
    fn auto_ignores_a_riser_with_no_low_end() {
        let mut sw = OnsetSwitcher::new();
        feed_calm(&mut sw, MIN_HOLD + 1.);
        // Rising flux, rising overall level, but the bass never arrives — a
        // sweep leading somewhere, not the thing it leads to.
        let mut fired = false;
        for i in 0..90 {
            let ramp = i as f32 / 90.;
            fired |= sw.observe(
                Onset {
                    flux: 0.05 + 0.2 * ramp,
                    // The sweep lives in the mids; the low end never moves.
                    bass_flux: 0.004,
                    energy: 0.3 + 0.5 * ramp,
                    bass: 0.2,
                },
                1. / 60.,
            );
        }
        assert!(!fired, "switched on the run-up instead of the hit");
        // ...and when the drop lands, it goes.
        assert!(
            feed_hit(&mut sw, 0.3, 0.3),
            "missed the hit after the riser"
        );
    }

    #[test]
    fn a_bass_hit_alone_is_enough_to_switch() {
        let mut sw = OnsetSwitcher::new();
        feed_calm(&mut sw, MIN_HOLD + 1.);
        // Kick returns: the low end jumps while the rest of the spectrum
        // barely moves. That is a scene change.
        assert!(
            feed_hit(&mut sw, 0.02, 0.35),
            "a bass hit should carry the switch on its own"
        );
    }

    #[test]
    fn auto_picks_scenes_in_a_varying_order() {
        let mut v = Visualizer::new(SpectrumTap::new());
        let mut seen = vec![v.scene];
        for _ in 0..14 {
            // Backstop fires deterministically, so this measures the choice of
            // scene rather than the detector.
            v.switcher.since = MAX_HOLD + 1.;
            v.advance(VisualizerMode::Auto, 1. / 60.);
            seen.push(v.scene);
        }
        assert!(
            seen.windows(2).all(|w| w[0] != w[1]),
            "repeated a scene back to back: {seen:?}"
        );
        assert!(
            seen.windows(3).all(|w| w[0] != w[2]),
            "ping-ponged between two scenes: {seen:?}"
        );
        // A fixed rotation would make every step from a given scene identical.
        let after_terrain: Vec<Scene> = seen
            .windows(2)
            .filter(|w| w[0] == Scene::Terrain)
            .map(|w| w[1])
            .collect();
        assert!(
            after_terrain.windows(2).any(|w| w[0] != w[1]),
            "always leaves Terrain for the same scene: {seen:?}"
        );
    }

    #[test]
    fn auto_ignores_onsets_in_silence() {
        let mut sw = OnsetSwitcher::new();
        feed_calm(&mut sw, MIN_HOLD + 1.);
        assert!(
            !sw.observe(
                Onset {
                    flux: 0.3,
                    bass_flux: 0.3,
                    energy: 0.0,
                    bass: 0.5
                },
                1. / 60.
            ),
            "flux with no energy is not music"
        );
    }

    #[test]
    fn auto_switches_eventually_without_any_onset() {
        let mut sw = OnsetSwitcher::new();
        // Perfectly flat input never produces a peak; MAX_HOLD is the backstop.
        let mut fired_at = None;
        for i in 0..(60 * (MAX_HOLD as usize + 2)) {
            if sw.observe(
                Onset {
                    flux: 0.01,
                    bass_flux: 0.01,
                    energy: 0.4,
                    bass: 0.2,
                },
                1. / 60.,
            ) {
                fired_at = Some(i as f32 / 60.);
                break;
            }
        }
        let t = fired_at.expect("never switched");
        assert!(t >= MAX_HOLD - 0.5, "switched early, at {t}s");
    }

    #[test]
    fn auto_advances_the_scene_and_starts_a_fade() {
        let mut v = Visualizer::new(SpectrumTap::new());
        assert_eq!(v.scene, Scene::Terrain);
        // Drive it through the backstop rather than a synthetic onset: the
        // flux `advance` acts on comes from the tap, so bands written here
        // would be overwritten before they were read.
        v.switcher.since = MAX_HOLD + 1.;
        v.advance(VisualizerMode::Auto, 1. / 60.);
        assert_ne!(v.scene, Scene::Terrain, "scene did not change");
        assert_eq!(v.fading, Some(Scene::Terrain));
        assert!(v.fade_left > 0.);
    }

    #[test]
    fn pinned_mode_never_switches_by_itself() {
        let mut v = Visualizer::new(SpectrumTap::new());
        v.switcher.since = MAX_HOLD + 10.;
        v.raw.iter_mut().for_each(|b| *b = 1.);
        for _ in 0..120 {
            v.advance(VisualizerMode::Sphere, 1. / 60.);
        }
        assert_eq!(v.scene, Scene::Sphere);
    }

    /// Synthetic track: quiet verses, a 1.5s riser into each drop, then a
    /// loud section. The riser is the point — it is what used to make the
    /// switch fire early. Returns the times (seconds) the scene changed.
    fn run_track(seconds: f32, drops: &[f32]) -> Vec<f32> {
        const RISER: f32 = 1.5;
        let tap = SpectrumTap::new();
        let mut v = Visualizer::new(tap.clone());
        let rate = 44_100f32;
        let per_frame = (rate / 60.) as usize;
        let mut n = 0usize;
        let mut noise = 987u32;
        let mut switches = Vec::new();
        let mut last = v.scene;
        let frames = (seconds * 60.) as usize;
        for f in 0..frames {
            let t0 = f as f32 / 60.;
            let since_drop = drops
                .iter()
                .filter(|d| **d <= t0)
                .map(|d| t0 - d)
                .fold(f32::MAX, f32::min);
            // How far into a riser we are, 0..1, if one is running.
            let ramp = drops
                .iter()
                .filter(|d| t0 < **d && t0 >= **d - RISER)
                .map(|d| 1. - (*d - t0) / RISER)
                .fold(0f32, f32::max);
            let hit = if since_drop == f32::MAX {
                0.
            } else {
                (-since_drop * 3.).exp()
            };
            for _ in 0..per_frame {
                let t = n as f32 / rate;
                noise = noise.wrapping_mul(1664525).wrapping_add(1013904223);
                let white = (noise >> 8) as f32 / 8_388_608. - 1.;
                let bass = 0.25 * (std::f32::consts::TAU * 55. * t).sin();
                // Riser: a sweep climbing in pitch, moving the spectrum a lot
                // while staying moderate in level — high flux, low energy.
                let riser = if ramp > 0. {
                    0.28 * ramp * (std::f32::consts::TAU * (400. + 3000. * ramp) * t).sin()
                } else {
                    0.
                };
                let body = 0.7 * hit * (white + (std::f32::consts::TAU * 900. * t).sin());
                tap.push_for_test(bass * (0.3 + 0.7 * hit) + riser + body + 0.03 * white);
                n += 1;
            }
            v.advance(VisualizerMode::Auto, 1. / 60.);
            if v.scene != last {
                switches.push(t0);
                last = v.scene;
            }
        }
        switches
    }

    #[test]
    fn auto_switches_land_on_the_drops_and_never_on_the_run_up() {
        const RISER: f32 = 1.5;
        let drops = [10., 24.];
        let switches = run_track(34., &drops);
        assert!(!switches.is_empty(), "never switched on a track with drops");
        assert!(
            switches.iter().all(|t| *t >= MIN_HOLD),
            "switched inside the opening hold: {switches:?}"
        );
        // Each drop gets a switch, promptly.
        for d in &drops {
            let hit = switches.iter().any(|t| *t >= *d && *t - *d < 0.4);
            assert!(hit, "no switch just after the drop at {d}s: {switches:?}");
        }
        // And nothing fires during a riser. This is the regression: flux peaks
        // on the run-up, so a detector that fires on the threshold crossing
        // switches before the beat instead of on it.
        for t in &switches {
            let early = drops.iter().any(|d| *t < *d && *t >= *d - RISER);
            assert!(!early, "switch at {t}s landed on a riser: {switches:?}");
        }
    }

    #[test]
    fn silence_decays_the_bands() {
        let mut v = Visualizer::new(SpectrumTap::new());
        v.bands.iter_mut().for_each(|b| *b = 1.);
        // No samples ever written, so every step sees an unchanged head.
        for _ in 0..120 {
            v.advance(VisualizerMode::Terrain, 1. / 60.);
        }
        assert!(v.energy() < 0.01, "energy stayed at {}", v.energy());
    }

    #[test]
    fn audio_drives_the_bands_up() {
        let tap = SpectrumTap::new();
        let mut v = Visualizer::new(tap.clone());
        let rate = tap.sample_rate() as f32;
        // Feed a frame's worth of samples per step, as playback does — the tap
        // only reports fresh audio when its counter moves.
        let mut i = 0usize;
        for _ in 0..30 {
            for _ in 0..(rate as usize / 60) {
                tap.push_for_test((2. * std::f32::consts::PI * 440. * i as f32 / rate).sin());
                i += 1;
            }
            v.advance(VisualizerMode::Terrain, 1. / 60.);
        }
        let peak = v.bands.iter().cloned().fold(0f32, f32::max);
        assert!(peak > 0.5, "loudest band only reached {peak}");
    }

    #[test]
    fn history_scrolls_at_a_fixed_rate_not_per_frame() {
        let mut v = Visualizer::new(SpectrumTap::new());
        v.bands.iter_mut().for_each(|b| *b = 0.5);
        // Ten frames far shorter than ROW_INTERVAL must not push ten rows.
        for _ in 0..10 {
            v.advance(VisualizerMode::Terrain, ROW_INTERVAL / 10.);
        }
        assert_eq!(v.history.len(), ROWS);
        let pushed = v.history.iter().filter(|r| r[0] > 0.01).count();
        assert_eq!(pushed, 1, "expected one row, got {pushed}");
    }

    #[test]
    fn behind_the_near_plane_does_not_project() {
        let bounds = Bounds {
            origin: point(px(0.), px(0.)),
            size: gpui::size(px(800.), px(600.)),
        };
        assert!(
            project(
                P3 {
                    x: 0.,
                    y: 0.,
                    z: -1.
                },
                bounds,
                100.
            )
            .is_none()
        );
        let front = project(
            P3 {
                x: 0.,
                y: 0.,
                z: 1.,
            },
            bounds,
            100.,
        )
        .expect("in front of the camera");
        // Dead centre of the viewport.
        assert_eq!(front, point(px(400.), px(300.)));
    }
}
