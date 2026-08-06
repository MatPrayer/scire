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

use crate::config::{VisualizerMode, VisualizerSettings};

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
    Scope,
    Bloom,
    Warp,
}

impl Scene {
    const ALL: [Scene; 8] = [
        Scene::Terrain,
        Scene::Tunnel,
        Scene::Sphere,
        Scene::Retro,
        Scene::Orb,
        Scene::Scope,
        Scene::Bloom,
        Scene::Warp,
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
        VisualizerMode::Scope => Some(Scene::Scope),
        VisualizerMode::Bloom => Some(Scene::Bloom),
        VisualizerMode::Warp => Some(Scene::Warp),
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
    /// User tuning: >1 lowers the thresholds, <1 raises them.
    sensitivity: f32,
    /// User tuning: seconds to ignore onsets after a switch (`MIN_HOLD`).
    min_hold: f32,
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
            sensitivity: 1.,
            min_hold: MIN_HOLD,
        }
    }

    /// Apply the user's tuning. Kept separate from `observe` so the detection
    /// path stays a pure function of the audio and the tests can drive it
    /// without carrying settings around.
    fn tune(&mut self, sensitivity: f32, min_hold: f32) {
        self.sensitivity = sensitivity.clamp(0.2, 4.);
        self.min_hold = min_hold.clamp(1., MAX_HOLD);
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
        if self.since < self.min_hold || onset.energy < 0.02 || self.hist.len() < 30 {
            self.armed = None;
            return false;
        }

        let n = self.hist.len() as f32;
        let mean = self.hist.iter().sum::<f32>() / n;
        let sd = (self.hist.iter().map(|f| (f - mean).powi(2)).sum::<f32>() / n).sqrt();
        let bass_mean = self.bass_hist.iter().sum::<f32>() / n;

        match &mut self.armed {
            None => {
                // Sensitivity scales the whole threshold, floor included: a
                // sigma-only scaling would leave the ratio and floor terms
                // dominant and the slider would do nothing on steady tracks.
                let arm_at = (mean + FLUX_SIGMAS * sd).max(mean * FLUX_RATIO) / self.sensitivity;
                if strength > arm_at && strength > FLUX_FLOOR / self.sensitivity {
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
                let hard =
                    (mean + FLUX_SIGMAS_HARD * sd).max(mean * FLUX_RATIO_HARD) / self.sensitivity;
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
    /// Tunnel's forward travel. Accumulated rather than derived from `spin`,
    /// because the speed rides on the track's energy and multiplying a growing
    /// clock by a changing rate would jump the rings whenever it changed.
    tunnel_z: f32,
    /// Scope scene: the last few triggered waveform traces, newest at the
    /// front. The scene draws the older ones as fading trails, which is what
    /// turns a flat line into a sense of motion.
    scope: VecDeque<Vec<f32>>,
    /// Warp scene: the star field, persistent across frames.
    stars: Vec<Star>,
    /// This frame's warp speed in world units per second. Kept here rather than
    /// recomputed in the paint pass so the streak drawn behind each star is
    /// exactly the distance it is about to travel.
    warp_speed: f32,
    /// User tuning, refreshed from settings on every `tick`.
    tuning: VisualizerSettings,
}

/// One star in the Warp scene. Streak length comes from the shared
/// `warp_speed`, so a star only needs where it is and what colours it.
#[derive(Clone, Copy)]
struct Star {
    /// Position on the plane the field flies through, in world units.
    x: f32,
    y: f32,
    /// Depth toward the camera.
    z: f32,
    /// Which band brightens this star, so the field twinkles with the music
    /// rather than uniformly.
    band: usize,
    /// Hue offset from the accent.
    hue: f32,
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

/// Points kept in one Scope trace. Enough to show a couple of cycles of a bass
/// note without the ring turning into a solid band of ink.
const SCOPE_POINTS: usize = 200;
/// Traces kept for the Scope trails, including the live one.
const SCOPE_TRAILS: usize = 7;
/// Samples skipped between Scope points. `SCOPE_POINTS * SCOPE_STRIDE` samples
/// — ~18ms at 44.1kHz — is the window the ring shows: long enough for a couple
/// of cycles of a bass note, short enough that the shape is legible.
const SCOPE_STRIDE: usize = 4;
/// Stars in flight in the Warp scene.
const STARS: usize = 260;
/// Depth a recycled star re-enters at, and the one at which it has passed the
/// camera. Deeper than the Retro field: streaks need room to stretch.
const STAR_FAR: f32 = 11.;
const STAR_NEAR: f32 = 0.28;

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
            tunnel_z: 0.,
            scope: VecDeque::from(vec![vec![0.; SCOPE_POINTS]; SCOPE_TRAILS]),
            stars: Vec::new(),
            warp_speed: 0.,
            tuning: VisualizerSettings::default(),
        };
        // Spread the initial field through the depth range so the scene is
        // already populated the first time it is shown.
        for i in 0..SHAPES {
            let mut shape = viz.spawn_shape();
            shape.z = SHAPE_NEAR + (SHAPE_FAR - SHAPE_NEAR) * (i as f32 / SHAPES as f32);
            viz.shapes.push(shape);
        }
        for i in 0..STARS {
            let mut star = viz.spawn_star();
            star.z = STAR_NEAR + (STAR_FAR - STAR_NEAR) * (i as f32 / STARS as f32);
            viz.stars.push(star);
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

    /// A star somewhere on the far plane. Placed by angle and radius rather
    /// than in a box: a uniform square puts the densest part of the field in
    /// the corners, where the streaks leave the frame immediately, and leaves
    /// the middle — the only part the eye tracks — sparse.
    fn spawn_star(&mut self) -> Star {
        let (a, r, b, h) = (self.rand(), self.rand(), self.rand(), self.rand());
        let angle = a * std::f32::consts::TAU;
        // sqrt keeps the area density even; the floor keeps stars off the exact
        // vanishing point, where the projection stretches them into needles.
        let radius = 0.25 + 3.1 * r.sqrt();
        Star {
            x: angle.cos() * radius,
            y: angle.sin() * radius,
            z: STAR_FAR,
            band: (b * BANDS as f32) as usize % BANDS,
            hue: (h - 0.5) * 0.16,
        }
    }

    /// One triggered waveform trace for the Scope scene, decimated from the
    /// sample buffer.
    ///
    /// The trace starts at a rising zero crossing rather than at the head of
    /// the buffer: without that trigger the waveform slides sideways by a
    /// different amount every frame — the classic untriggered-oscilloscope
    /// smear — and the ring never holds still long enough to read as a shape.
    ///
    /// Samples are taken directly at `SCOPE_STRIDE`, not reduced from buckets:
    /// a peak-per-bucket reduction draws the signal's *envelope*, which is
    /// steadier but is no longer a waveform — the ring stops crossing its own
    /// baseline and the scene turns into a second spectrum display.
    fn scope_trace(&self) -> Vec<f32> {
        let window = SCOPE_POINTS * SCOPE_STRIDE;
        // Only look for the trigger in the part of the buffer that still leaves
        // a full window behind it.
        let limit = self.samples.len().saturating_sub(window).max(1);
        let start = (1..limit)
            .find(|&i| self.samples[i - 1] <= 0. && self.samples[i] > 0.)
            .unwrap_or(0);
        (0..SCOPE_POINTS)
            .map(|i| {
                self.samples
                    .get(start + i * SCOPE_STRIDE)
                    .copied()
                    .unwrap_or(0.)
            })
            .collect()
    }

    /// Read the newest samples, run the FFT, and advance the animation clocks.
    /// Call once per rendered frame, with the mode currently selected.
    pub fn tick(&mut self, mode: VisualizerMode, tuning: VisualizerSettings) {
        self.tuning = tuning;
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
        let motion = self.tuning.motion.clamp(0.1, 3.);
        self.spin += dt * motion;

        let head = self.tap.snapshot(&mut self.samples);
        let silent = head == self.last_head;
        if silent {
            // Paused or starved: fall to silence rather than hold the last
            // frame, which would look frozen rather than quiet.
            self.raw.iter_mut().for_each(|b| *b = 0.);
        } else {
            spectrum::analyze(&self.samples, &mut self.raw, self.tap.sample_rate());
        }
        self.last_head = head;

        // Fast attack, slow release: peaks land on the beat, decay reads as
        // the note ringing out instead of flickering. The smoothing knob slides
        // both rates; 0.5 reproduces the hand-tuned 26/7.
        let smooth = self.tuning.smoothing.clamp(0., 1.);
        let attack = 1. - (-dt * (40. - 28. * smooth)).exp();
        let release = 1. - (-dt * (12. - 10. * smooth)).exp();
        // Gain is applied to the drawn bands only, not to `raw`: the onset
        // detector's thresholds are calibrated against the ungained flux, and
        // scaling both sides of a ratio test would change nothing anyway.
        let gain = self.tuning.sensitivity.clamp(0.2, 4.);
        for (out, &target) in self.bands.iter_mut().zip(self.raw.iter()) {
            let target = (target * gain).min(1.);
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
        let speed = (0.55 + 1.9 * self.energy()) * motion;
        for i in 0..self.shapes.len() {
            self.shapes[i].z -= speed * dt;
            for axis in 0..3 {
                self.shapes[i].rot[axis] += self.shapes[i].rot_rate[axis] * dt;
            }
            if self.shapes[i].z <= SHAPE_NEAR {
                self.shapes[i] = self.spawn_shape();
            }
        }

        // Tunnel travel: faster when the track is busy, so a drop reads as
        // acceleration down the tube. Wrapped to one ring spacing in the paint
        // pass, so this can grow without losing precision for hours.
        self.tunnel_z += dt * (0.85 + 1.6 * self.energy()) * motion;

        // Warp: the field accelerates with the track and is kicked by the beat
        // flash, so a drop reads as the streaks stretching out rather than as
        // the stars merely getting brighter.
        self.warp_speed = (1.5 + 5.5 * self.energy() + 3.5 * self.flash) * motion;
        for i in 0..self.stars.len() {
            self.stars[i].z -= self.warp_speed * dt;
            if self.stars[i].z <= STAR_NEAR {
                self.stars[i] = self.spawn_star();
            }
        }

        // Scope: one trace per frame, oldest dropped. Silence pushes a flat
        // line rather than repeating the last trace, so the trails drain away
        // instead of freezing mid-waveform.
        let trace = if silent {
            vec![0.; SCOPE_POINTS]
        } else {
            self.scope_trace()
        };
        self.scope.pop_back();
        self.scope.push_front(trace);

        self.fade_left = (self.fade_left - dt).max(0.);
        if self.fade_left == 0. {
            self.fading = None;
        }

        self.switcher
            .tune(self.tuning.switch_sensitivity, self.tuning.switch_hold);
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

        self.row_accum += dt * motion;
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
        // How far the audio is allowed to deform each scene.
        let power = self.tuning.intensity.clamp(0.1, 3.);
        let tunnel_z = self.tunnel_z;
        let scope: Vec<Vec<f32>> = self.scope.iter().cloned().collect();
        let stars = self.stars.clone();
        let warp_speed = self.warp_speed;
        // The Scope draws the waveform itself, which never went through the
        // band gain, so it has to apply the sensitivity knob on its own.
        let gain = self.tuning.sensitivity.clamp(0.2, 4.);

        canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                let mut draw = |scene: Scene, alpha: f32| match scene {
                    Scene::Terrain => paint_terrain(bounds, &history, power, accent, alpha, window),
                    Scene::Tunnel => paint_tunnel(
                        bounds, &bands, spin, tunnel_z, energy, power, accent, alpha, window,
                    ),
                    Scene::Sphere => {
                        paint_sphere(bounds, &bands, spin, energy, power, accent, alpha, window)
                    }
                    Scene::Retro => paint_retro(
                        bounds, &bands, &shapes, flash, bass, power, accent, alpha, window,
                    ),
                    Scene::Orb => paint_orb(
                        bounds, &orb, spin, bass, treble, power, accent, alpha, window,
                    ),
                    Scene::Scope => paint_scope(
                        bounds, &scope, spin, gain, bass, flash, power, accent, alpha, window,
                    ),
                    Scene::Bloom => paint_bloom(
                        bounds, &bands, spin, bass, flash, power, accent, alpha, window,
                    ),
                    Scene::Warp => paint_warp(
                        bounds, &stars, &bands, spin, warp_speed, flash, power, accent, alpha,
                        window,
                    ),
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

/// Terrain camera constants, shared with the geometry test below.
const TERRAIN_ZOOM: f32 = 0.55;
const TERRAIN_Z_NEAR: f32 = 1.7;
const TERRAIN_Z_STEP: f32 = 0.28;
/// Half-width of the nearest row, in world units.
const TERRAIN_HALF_W: f32 = 2.0;
/// Extra half-width per unit of depth. The projected half-span of a row tends
/// to `TERRAIN_WIDEN * focal` as z grows, so this sets how far the far rows
/// overhang the viewport; below ~0.95 they end inside it.
const TERRAIN_WIDEN: f32 = 1.05;

/// World half-width of the terrain row at depth `z`. Rows widen with depth
/// instead of being a constant-width grid: a constant width shrinks on screen
/// as 1/z, so the far rows would end well inside the viewport and the landscape
/// would read as a floating carpet with visible corners. Widening cancels most
/// of that divide, so every row runs off both edges of the window.
fn terrain_row_half(z: f32) -> f32 {
    TERRAIN_HALF_W + TERRAIN_WIDEN * (z - TERRAIN_Z_NEAR)
}

/// Scrolling spectrum landscape: frequency across x, time receding into z,
/// level as height. Rows are drawn far to near, each filled below its ridge so
/// it hides the rows behind it.
fn paint_terrain(
    bounds: Bounds<Pixels>,
    history: &[Vec<f32>],
    power: f32,
    accent: Hsla,
    alpha: f32,
    window: &mut gpui::Window,
) {
    let focal = focal_of(bounds, TERRAIN_ZOOM);
    let base_y = -0.5;
    let height = 1.05 * power;
    let z_near = TERRAIN_Z_NEAR;
    let z_step = TERRAIN_Z_STEP;
    // Rows are filled from their ridge down to well below the frame: an
    // exactly-baseline fill leaves slits of background between rows, which
    // breaks the illusion that they are a continuous surface.
    let floor_y = -12.;

    for (r, row) in history.iter().enumerate().rev() {
        let z = z_near + r as f32 * z_step;
        let depth = r as f32 / history.len().max(1) as f32;
        let row_half = terrain_row_half(z);
        let x_at = |b: usize| (b as f32 / (BANDS - 1) as f32 - 0.5) * 2. * row_half;

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
#[allow(clippy::too_many_arguments)]
fn paint_tunnel(
    bounds: Bounds<Pixels>,
    bands: &[f32],
    spin: f32,
    travel: f32,
    energy: f32,
    power: f32,
    accent: Hsla,
    alpha: f32,
    window: &mut gpui::Window,
) {
    const RINGS: usize = 22;
    const SEGMENTS: usize = 96;
    let focal = focal_of(bounds, 0.7);
    // Near plane close to the camera and rings packed tighter than they are
    // wide: the tube swallows the frame and the walls rush past instead of
    // sitting in the middle of the window as a ribbed disc.
    let z_near = 0.75;
    let z_step = 0.52;
    // Forward drift; `fract` recycles the nearest ring to the back. `travel`
    // is accumulated per frame (see `Visualizer::tunnel_z`) so the speed can
    // ride on the track without the ring positions jumping when it changes.
    let drift = travel.fract();

    for i in (0..RINGS).rev() {
        let z = z_near + (i as f32 + drift) * z_step;
        let depth = i as f32 / RINGS as f32;
        // Rings turn as a whole, with only a slight lean per ring: twisting
        // hard with depth makes a single loud band spiral across the rings and
        // read as a stray diagonal streak rather than as a ridge in the tube.
        let twist = spin * 0.42 + z * 0.09;

        let mut pb = PathBuilder::stroke(px(3.2 - 2.3 * depth));
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
            // Deep radial modulation: the wall is meant to be pushed into
            // ridges by the spectrum, not gently rippled.
            let radius = 0.46 * (1. + 1.25 * level * power);
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
            // Steep falloff: the far rings crowd into a few pixels around the
            // vanishing point, and at a flat alpha they pile up into a moiré
            // knot instead of reading as distance.
            let a = (1. - depth).powf(1.7) * (0.5 + 0.5 * energy) * alpha;
            window.paint_path(path, depth_color(accent, depth, a));
        }
    }
}

/// Rotating point cloud on a Fibonacci sphere, each point pushed out along its
/// normal by the band it belongs to. Points are quads sized by depth, which is
/// both cheaper than paths and reads as a proper 3D cloud.
#[allow(clippy::too_many_arguments)]
fn paint_sphere(
    bounds: Bounds<Pixels>,
    bands: &[f32],
    spin: f32,
    energy: f32,
    power: f32,
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
        let radius = 0.75 * (1. + 0.55 * band * power);
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
    power: f32,
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
        let size = shape.scale * (0.7 + 0.9 * level * power);
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
    power: f32,
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

    let radius = 0.58 * (1. + 0.42 * bass * power);
    let rough = 0.10 + 0.62 * treble * power;
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

/// Fraction of the Scope ring over which the waveform's displacement is faded
/// in and out. The trace starts and ends at unrelated sample values, so without
/// this the ring has a visible step where the buffer wraps.
const SCOPE_SEAM: f32 = 0.06;
/// Fraction of *this trace's own peak* past which a stretch is redrawn
/// brighter. A real scope's phosphor blooms where the beam swings hardest; this
/// is the cheap equivalent.
///
/// Relative rather than absolute: at any fixed threshold a loud passage puts
/// nearly the whole waveform over it, every point is drawn hot, and the effect
/// disappears exactly when it should be strongest — the crests have to be
/// picked out against the rest of the *same* trace.
const SCOPE_HOT_REL: f32 = 0.7;
/// Floor under the relative threshold. Without it a near-silent trace still has
/// a peak, and its noise crests would glow as brightly as a snare.
const SCOPE_HOT_FLOOR: f32 = 0.15;
/// Resolution of the baseline circle. Low — it is a reference line, not
/// geometry, and at this radius the eye cannot tell 64 segments from 200.
const SCOPE_BASELINE_STEPS: usize = 64;

/// Index ranges of the trace swinging within `SCOPE_HOT_REL` of its own peak.
///
/// Each run is padded by one point on both sides so the overlay meets the base
/// stroke instead of floating as a detached arc. Returned as ranges rather than
/// drawn per point because `paint_path` takes a single colour: to have part of
/// a trace glow, that part has to be its own path.
fn hot_runs(trace: &[f32], gain: f32) -> Vec<(usize, usize)> {
    let peak = trace
        .iter()
        .fold(0f32, |acc, &s| acc.max((s * gain).abs().min(1.)));
    let threshold = (peak * SCOPE_HOT_REL).max(SCOPE_HOT_FLOOR);
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut start: Option<usize> = None;
    for i in 0..trace.len() {
        let hot = (trace[i] * gain).abs() >= threshold;
        match (hot, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                runs.push((s.saturating_sub(1), (i).min(trace.len() - 1)));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        runs.push((s.saturating_sub(1), trace.len() - 1));
    }
    runs
}

/// Polar oscilloscope: the waveform wrapped around a ring, with the previous
/// frames trailing behind it as fading echoes.
///
/// This is the only scene that draws the time domain — everything else works
/// from the FFT — so a plucked string reads as a shape here rather than as a
/// level. The trace is triggered on a zero crossing in `scope_trace`; without
/// that the ring rotates by a random phase every frame.
#[allow(clippy::too_many_arguments)]
fn paint_scope(
    bounds: Bounds<Pixels>,
    scope: &[Vec<f32>],
    spin: f32,
    gain: f32,
    bass: f32,
    flash: f32,
    power: f32,
    accent: Hsla,
    alpha: f32,
    window: &mut gpui::Window,
) {
    let w = f32::from(bounds.size.width);
    let h = f32::from(bounds.size.height);
    let cx = f32::from(bounds.origin.x) + w / 2.;
    let cy = f32::from(bounds.origin.y) + h / 2.;
    // The ring itself jumps on the beat. Everything else here is driven by the
    // waveform, which carries no sense of *tempo* on its own — a steady tone
    // and a kick drum draw the same size of ring.
    let base = w.min(h) * 0.28 * (1. + 0.05 * flash);
    // The waveform is a signed displacement about the ring, so it needs room on
    // both sides; deep enough that a loud passage nearly closes the middle.
    let swing = base * 0.85 * power;

    // Core: a disc breathing on the low end, so the ring has something to sit
    // around and the middle of the frame is not a hole.
    let core = base * (0.34 + 0.22 * bass);
    let core_box = Bounds {
        origin: point(px(cx - core), px(cy - core)),
        size: gpui::size(px(core * 2.), px(core * 2.)),
    };
    window.paint_quad(
        fill(
            core_box,
            Hsla {
                a: (0.05 + 0.14 * bass) * alpha,
                ..accent
            },
        )
        .corner_radii(px(core)),
    );

    // Baseline: the zero-displacement circle the trace swings about. An
    // oscilloscope without one shows a shape with nothing to read it against —
    // during a quiet passage the ring shrinks to almost this circle, and the
    // eye needs to see that it is *near* zero rather than merely small.
    {
        let mut pb = PathBuilder::stroke(px(1.));
        for i in 0..=SCOPE_BASELINE_STEPS {
            let a = i as f32 / SCOPE_BASELINE_STEPS as f32 * std::f32::consts::TAU + spin * 0.18;
            let (sin, cos) = a.sin_cos();
            let pt = point(px(cx + cos * base), px(cy - sin * base));
            if i == 0 {
                pb.move_to(pt);
            } else {
                pb.line_to(pt);
            }
        }
        if let Ok(path) = pb.build() {
            window.paint_path(
                path,
                Hsla {
                    a: (0.10 + 0.10 * flash) * alpha,
                    ..accent
                },
            );
        }
    }

    // Oldest first: the live trace has to land on top of its own history.
    for (age, trace) in scope.iter().enumerate().rev() {
        if trace.len() < 3 {
            continue;
        }
        let fade = age as f32 / scope.len() as f32;
        // Echoes drift outward as they fade, which is what makes the trail read
        // as motion rather than as a blurred copy of the same ring.
        let ring = base * (1. + 0.16 * fade);
        let mut pb = PathBuilder::stroke(px(2.4 - 1.7 * fade));
        let mut started = false;
        for (i, &s) in trace.iter().enumerate() {
            let u = i as f32 / trace.len() as f32;
            // Taper both ends of the buffer to zero displacement so the ring
            // closes on itself smoothly.
            let seam = (u / SCOPE_SEAM).min((1. - u) / SCOPE_SEAM).clamp(0., 1.);
            let a = u * std::f32::consts::TAU + spin * 0.18;
            let r = ring + (s * gain).clamp(-1., 1.) * swing * seam;
            let (sin, cos) = a.sin_cos();
            let pt = point(px(cx + cos * r), px(cy - sin * r));
            if started {
                pb.line_to(pt);
            } else {
                pb.move_to(pt);
                started = true;
            }
        }
        if !started {
            continue;
        }
        pb.close();
        if let Ok(path) = pb.build() {
            window.paint_path(
                path,
                Hsla {
                    h: (accent.h + 0.05 * fade).rem_euclid(1.),
                    s: accent.s,
                    l: (accent.l * (1. - 0.3 * fade) + 0.12).clamp(0., 1.),
                    a: (1. - fade).powf(1.6) * alpha,
                },
            );
        }

        // Phosphor bloom, live trace only: the stretches that swing hardest are
        // redrawn thicker and near-white. Without it every part of the trace
        // carries the same weight and a transient is indistinguishable from a
        // sustained tone at the same level.
        if age != 0 {
            continue;
        }
        for (from, to) in hot_runs(trace, gain) {
            if to <= from {
                continue;
            }
            let mut hot = PathBuilder::stroke(px(4.2));
            for i in from..=to {
                let u = i as f32 / trace.len() as f32;
                let seam = (u / SCOPE_SEAM).min((1. - u) / SCOPE_SEAM).clamp(0., 1.);
                let a = u * std::f32::consts::TAU + spin * 0.18;
                let r = ring + (trace[i] * gain).clamp(-1., 1.) * swing * seam;
                let (sin, cos) = a.sin_cos();
                let pt = point(px(cx + cos * r), px(cy - sin * r));
                if i == from {
                    hot.move_to(pt);
                } else {
                    hot.line_to(pt);
                }
            }
            if let Ok(path) = hot.build() {
                window.paint_path(
                    path,
                    Hsla {
                        h: accent.h,
                        s: (accent.s * 0.6).clamp(0., 1.),
                        l: (accent.l + 0.42).min(0.97),
                        a: (0.5 + 0.4 * flash) * alpha,
                    },
                );
            }
        }
    }
}

/// Sectors each Bloom layer is mirrored into, outermost first. All even, since
/// the mirroring pairs sectors up.
///
/// Deliberately *different* per layer: three layers sharing one petal count
/// line up into a single rigid figure no matter how they rotate, and the
/// interference between symmetries that don't divide each other is the thing
/// that reads as a kaleidoscope rather than as a gear.
const BLOOM_LAYER_SECTORS: [usize; 3] = [12, 8, 6];
/// Concentric layers, each rotating against the one outside it.
const BLOOM_LAYERS: usize = BLOOM_LAYER_SECTORS.len();
/// Outline resolution of one petal.
const BLOOM_STEPS: usize = 24;

/// Kaleidoscope mandala: the spectrum folded into mirrored petals that turn
/// against each other.
///
/// Screen space, not projected — the whole point of a kaleidoscope is exact
/// radial symmetry, and perspective would break it. Alternate sectors read the
/// spectrum backwards, which is what makes neighbouring petals mirror instead
/// of merely repeat.
#[allow(clippy::too_many_arguments)]
fn paint_bloom(
    bounds: Bounds<Pixels>,
    bands: &[f32],
    spin: f32,
    bass: f32,
    flash: f32,
    power: f32,
    accent: Hsla,
    alpha: f32,
    window: &mut gpui::Window,
) {
    let w = f32::from(bounds.size.width);
    let h = f32::from(bounds.size.height);
    let cx = f32::from(bounds.origin.x) + w / 2.;
    let cy = f32::from(bounds.origin.y) + h / 2.;
    let outer = w.min(h) * 0.46;

    // Spokes on the beat: a thin star along the sector boundaries, gone within
    // a few frames. The only part of the scene that moves on the kick rather
    // than with the spectrum. Aligned to the outer layer, whose petals they
    // pass between, and drawn *under* everything: on top they read as scratches
    // ruled across the figure, and they must not reach past the petals or the
    // mandala grows spikes on every beat.
    if flash > 0.02 {
        let mut pb = PathBuilder::stroke(px(1.4));
        let sector = std::f32::consts::TAU / BLOOM_LAYER_SECTORS[0] as f32;
        for s in 0..BLOOM_LAYER_SECTORS[0] {
            let a = spin * 0.3 + s as f32 * sector;
            let (sin, cos) = a.sin_cos();
            let inner = outer * 0.22;
            let reach = outer * (0.5 + 0.32 * flash);
            pb.move_to(point(px(cx + cos * inner), px(cy - sin * inner)));
            pb.line_to(point(px(cx + cos * reach), px(cy - sin * reach)));
        }
        if let Ok(path) = pb.build() {
            window.paint_path(
                path,
                Hsla {
                    l: (accent.l + 0.25).min(0.95),
                    a: flash * 0.5 * alpha,
                    ..accent
                },
            );
        }
    }

    for layer in 0..BLOOM_LAYERS {
        let sectors = BLOOM_LAYER_SECTORS[layer];
        let sector = std::f32::consts::TAU / sectors as f32;
        let l = layer as f32 / BLOOM_LAYERS as f32;
        let scale = 1. - 0.3 * l;
        // Counter-rotation: layers turning the same way would lock into one
        // rigid figure, and the interference between directions is what gives
        // a kaleidoscope its shifting look.
        let dir = if layer % 2 == 0 { 1. } else { -1.4 };
        let rot = spin * 0.3 * dir + l * 0.7;
        // Inner layers read the top of the spectrum, outer ones the bottom, so
        // the mandala is not three copies of the same outline.
        let band_lo = l * 0.35;

        let mut pb = PathBuilder::fill();
        let mut outline = PathBuilder::stroke(px(1.8 - 0.5 * l));
        let mut started = false;
        for s in 0..sectors {
            // Mirrored pairs of sectors share one band, so the figure has a
            // true mirror symmetry with `sectors / 2` petals. Reading the whole
            // spectrum in every sector instead makes all the petals identical,
            // which is a cog, not a kaleidoscope.
            let pair = (s / 2) as f32 / (sectors / 2) as f32;
            let b = band_lo + pair * (1. - band_lo);
            // Smoothed across neighbours: an isolated loud band would drive one
            // petal on its own and the figure would lose its balance.
            let amp =
                (band_at(bands, b - 0.07) + 2. * band_at(bands, b) + band_at(bands, b + 0.07)) / 4.;
            for step in 0..=BLOOM_STEPS {
                let u = step as f32 / BLOOM_STEPS as f32;
                // Mirror odd sectors.
                let t = if s % 2 == 0 { u } else { 1. - u };
                // Lobe window: the radius has to return to the inner circle at
                // both edges of a sector, otherwise the petals merge into a
                // lumpy ring with no shape to it.
                //
                // Clamped before the exponent: at u=1 the sine lands a hair
                // below zero, and a negative base under a fractional power is
                // NaN — which reaches lyon as a non-finite point and aborts.
                let lobe = (u * std::f32::consts::PI).sin().max(0.).powf(0.65);
                // The petal's *length* comes from its band, its *shape* from
                // the lobe; the spectrum only ripples the edge. Shaping the
                // outline point by point instead traces every peak of a spiky
                // spectrum and the mandala comes out as a sea urchin.
                let level = 0.78 * amp + 0.22 * band_at(bands, band_lo + t * (1. - band_lo));
                // Inner radius well off the centre: with the petals starting
                // near the middle, all three layers pile up into one solid
                // blob and the symmetry — the only thing to look at here — is
                // buried under it.
                let r =
                    outer * scale * (0.34 + 0.1 * bass + 0.58 * (level * power).min(1.4) * lobe);
                let a = rot + (s as f32 + u) * sector;
                let (sin, cos) = a.sin_cos();
                let pt = point(px(cx + cos * r), px(cy - sin * r));
                if started {
                    pb.line_to(pt);
                    outline.line_to(pt);
                } else {
                    pb.move_to(pt);
                    outline.move_to(pt);
                    started = true;
                }
            }
        }
        if !started {
            continue;
        }
        pb.close();
        outline.close();
        let hue = (accent.h + 0.07 * l + 0.05 * flash).rem_euclid(1.);
        if let Ok(path) = pb.build() {
            window.paint_path(
                path,
                Hsla {
                    h: hue,
                    s: (accent.s * (0.75 + 0.25 * l)).clamp(0., 1.),
                    l: (accent.l * (0.55 + 0.45 * l) + 0.06 * flash).clamp(0., 1.),
                    // Layers are stacked, so each has to stay translucent for
                    // the ones behind it to show through as a second colour.
                    a: (0.22 - 0.04 * l + 0.16 * flash) * alpha,
                },
            );
        }
        // Lit edge over the translucent body: without it the overlapping
        // layers read as one wash and no single petal has a shape.
        if let Ok(path) = outline.build() {
            window.paint_path(
                path,
                Hsla {
                    h: hue,
                    s: accent.s,
                    l: (accent.l + 0.25 - 0.1 * l).min(0.92),
                    a: (0.65 - 0.15 * l) * alpha,
                },
            );
        }
    }

    // Hub: the petals all start well off the centre (they have to, or the
    // layers pile into a blob), which leaves a hole exactly where the eye goes
    // first. A disc on the low end and a ring at the petals' inner radius close
    // it and tie the three symmetries to a common centre.
    {
        let hub = outer * (0.13 + 0.10 * bass);
        window.paint_quad(
            fill(
                Bounds {
                    origin: point(px(cx - hub), px(cy - hub)),
                    size: gpui::size(px(hub * 2.), px(hub * 2.)),
                },
                Hsla {
                    l: (accent.l + 0.15 + 0.2 * flash).min(0.95),
                    a: (0.30 + 0.35 * flash) * alpha,
                    ..accent
                },
            )
            .corner_radii(px(hub)),
        );
        // Innermost layer's inner radius — where its petals spring from.
        let inner_scale = 1. - 0.3 * ((BLOOM_LAYERS - 1) as f32 / BLOOM_LAYERS as f32);
        let rim = outer * inner_scale * (0.34 + 0.1 * bass);
        let mut pb = PathBuilder::stroke(px(1.2));
        for i in 0..=SCOPE_BASELINE_STEPS {
            let a = i as f32 / SCOPE_BASELINE_STEPS as f32 * std::f32::consts::TAU;
            let (sin, cos) = a.sin_cos();
            let pt = point(px(cx + cos * rim), px(cy - sin * rim));
            if i == 0 {
                pb.move_to(pt);
            } else {
                pb.line_to(pt);
            }
        }
        if let Ok(path) = pb.build() {
            window.paint_path(
                path,
                Hsla {
                    l: (accent.l + 0.2).min(0.92),
                    a: (0.25 + 0.3 * flash) * alpha,
                    ..accent
                },
            );
        }
    }
}

/// Seconds of travel a warp streak represents. Fixed rather than the frame's
/// `dt`, so the streaks do not shorten when the frame rate rises.
const WARP_STREAK: f32 = 0.075;
/// Radians of field roll per unit of `spin`. Slow on purpose: a starfield that
/// only grows outward reads as a still image being zoomed, and it takes very
/// little rotation to turn that back into flight. Too much and it becomes a
/// spin, which is a different scene.
const WARP_ROLL: f32 = 0.09;
/// World units over which a star fades out as it reaches the camera. Without
/// it every streak vanishes mid-frame at the near plane — a pop in the busiest,
/// brightest part of the field.
const WARP_NEAR_FADE: f32 = 0.7;

/// Starfield streaking past the camera, accelerating with the track.
///
/// Each star is drawn as the segment it is about to travel, so speed shows up
/// as length: at rest the field is a still sky, and a drop stretches it into
/// the hyperspace look.
#[allow(clippy::too_many_arguments)]
fn paint_warp(
    bounds: Bounds<Pixels>,
    stars: &[Star],
    bands: &[f32],
    spin: f32,
    speed: f32,
    flash: f32,
    power: f32,
    accent: Hsla,
    alpha: f32,
    window: &mut gpui::Window,
) {
    let focal = focal_of(bounds, 0.75);
    let trail = speed * WARP_STREAK * power;
    // Rigid roll of the whole field: applied to the star's plane position, so
    // head and tail turn together and a streak still points at the vanishing
    // point instead of shearing into an arc.
    let (rs, rc) = (spin * WARP_ROLL).sin_cos();

    // Far first: near streaks are wider and brighter and have to sit on top.
    let mut order: Vec<&Star> = stars.iter().collect();
    order.sort_by(|a, b| b.z.partial_cmp(&a.z).unwrap_or(std::cmp::Ordering::Equal));

    for star in order {
        let (x, y) = (star.x * rc - star.y * rs, star.x * rs + star.y * rc);
        let head = P3 { x, y, z: star.z };
        let tail = P3 {
            x,
            y,
            z: star.z + trail,
        };
        let (Some(a), Some(b)) = (project(head, bounds, focal), project(tail, bounds, focal))
        else {
            continue;
        };
        let depth = ((star.z - STAR_NEAR) / (STAR_FAR - STAR_NEAR)).clamp(0., 1.);
        let near_fade = ((star.z - STAR_NEAR) / WARP_NEAR_FADE).clamp(0., 1.);
        let level = bands.get(star.band).copied().unwrap_or(0.);
        let width = (0.9 + 2.6 * (1. - depth) + 1.6 * level).min(4.5);
        let mut pb = PathBuilder::stroke(px(width));
        pb.move_to(b);
        pb.line_to(a);
        if let Ok(path) = pb.build() {
            window.paint_path(
                path,
                Hsla {
                    h: (accent.h + star.hue + 0.06 * flash).rem_euclid(1.),
                    s: (accent.s * (0.5 + 0.5 * depth)).clamp(0., 1.),
                    l: (0.45 + 0.4 * (1. - depth) + 0.25 * level).min(0.95),
                    // Fade in from the far plane so recycled stars appear out
                    // of the distance instead of popping into existence, and
                    // out again at the near plane where they are recycled.
                    a: (1. - depth).powf(0.7) * near_fade * (0.35 + 0.65 * level.min(1.)) * alpha,
                },
            );
        }
    }

    // No glow at the vanishing point. A disc is the only round shape available
    // without a radial gradient, and at the centre of a converging field it
    // reads as a solid ball parked in front of the camera rather than as
    // light. The streaks already point at where they came from.
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

    #[test]
    fn hot_runs_cover_the_loud_stretches_and_nothing_else() {
        // Two excursions past the threshold with quiet either side.
        let trace = vec![0., 0.1, 0.5, 0.6, 0.1, 0., -0.05, -0.7, -0.2];
        let runs = hot_runs(&trace, 1.);
        assert_eq!(runs.len(), 2, "{runs:?}");
        // Padded by one point on each side so the overlay meets the base line.
        assert_eq!(runs[0], (1, 4));
        assert_eq!(runs[1], (6, 8));
        // A trace that never swings hard has nothing to bloom.
        assert!(hot_runs(&[0., 0.05, -0.05, 0.], 1.).is_empty());
        // Gain is the sensitivity knob: turning it up makes the same trace hot.
        assert!(!hot_runs(&[0., 0.2, 0.], 4.).is_empty());
    }

    #[test]
    fn bloom_layers_can_be_mirrored() {
        // The petal pairing halves the sector count; an odd one would leave a
        // sector reading a band past the end of the spectrum.
        for n in BLOOM_LAYER_SECTORS {
            assert!(n % 2 == 0 && n >= 4, "unmirrorable sector count {n}");
        }
        // Layers sharing a count (or dividing each other evenly) lock into one
        // rigid figure — the interference is the scene.
        assert_ne!(BLOOM_LAYER_SECTORS[0], BLOOM_LAYER_SECTORS[1]);
        assert_ne!(BLOOM_LAYER_SECTORS[1], BLOOM_LAYER_SECTORS[2]);
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
    fn scope_trace_is_triggered_on_a_rising_zero_crossing() {
        let mut v = Visualizer::new(SpectrumTap::new());
        // A sine offset by a quarter cycle: the buffer starts at its peak, so
        // an untriggered trace would too.
        let period = 128.;
        for (i, s) in v.samples.iter_mut().enumerate() {
            *s = (i as f32 / period * std::f32::consts::TAU + std::f32::consts::FRAC_PI_2).sin();
        }
        let trace = v.scope_trace();
        assert_eq!(trace.len(), SCOPE_POINTS);
        assert!(
            trace[0].abs() < 0.2,
            "trace should start near a zero crossing, got {}",
            trace[0]
        );
        assert!(
            trace.iter().any(|s| *s > 0.8) && trace.iter().any(|s| *s < -0.8),
            "the trace should swing both sides of the baseline, not trace an envelope"
        );
    }

    #[test]
    fn silence_drains_the_scope_trails() {
        let mut v = Visualizer::new(SpectrumTap::new());
        v.samples.iter_mut().for_each(|s| *s = 0.7);
        // Nothing feeds the tap, so every frame reads as starved.
        for _ in 0..SCOPE_TRAILS + 2 {
            v.advance(VisualizerMode::Scope, 1. / 60.);
        }
        assert!(
            v.scope.iter().all(|t| t.iter().all(|s| *s == 0.)),
            "a starved tap should flatten the trails, not hold the last trace"
        );
    }

    #[test]
    fn warp_recycles_stars_that_pass_the_camera() {
        let mut v = Visualizer::new(SpectrumTap::new());
        // Long enough for the slowest star to cross the whole depth range.
        for _ in 0..600 {
            v.advance(VisualizerMode::Warp, 1. / 60.);
        }
        assert_eq!(v.stars.len(), STARS);
        assert!(
            v.stars
                .iter()
                .all(|s| s.z > STAR_NEAR && s.z <= STAR_FAR + 0.001),
            "a star escaped the depth range instead of being recycled"
        );
        assert!(
            v.stars.iter().any(|s| s.z < STAR_FAR * 0.5),
            "the field bunched at the far plane instead of spreading out"
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

    /// The point of widening the rows with depth: no row may end inside the
    /// window, at any depth or aspect ratio, or the landscape reads as a
    /// carpet with visible corners instead of ground running past the camera.
    #[test]
    fn every_terrain_row_runs_off_both_edges() {
        for (w, h) in [(1280., 720.), (2560., 1440.), (900., 1400.)] {
            let bounds = Bounds {
                origin: point(px(0.), px(0.)),
                size: gpui::size(px(w), px(h)),
            };
            let focal = focal_of(bounds, TERRAIN_ZOOM);
            for r in 0..ROWS {
                let z = TERRAIN_Z_NEAR + r as f32 * TERRAIN_Z_STEP;
                let edge = terrain_row_half(z) * focal / z;
                assert!(
                    edge > w / 2.,
                    "row {r} ends at {edge}px inside a {w}x{h} window"
                );
            }
        }
    }

    #[test]
    fn sensitivity_scales_the_drawn_bands() {
        // Same quiet tone into both, one with the gain turned up.
        let (tap_a, tap_b) = (SpectrumTap::new(), SpectrumTap::new());
        let mut quiet = Visualizer::new(tap_a.clone());
        let mut loud = Visualizer::new(tap_b.clone());
        loud.tuning.sensitivity = 3.0;
        let rate = tap_a.sample_rate() as f32;
        let mut i = 0usize;
        for _ in 0..30 {
            for _ in 0..(rate as usize / 60) {
                let s = 0.05 * (2. * std::f32::consts::PI * 440. * i as f32 / rate).sin();
                tap_a.push_for_test(s);
                tap_b.push_for_test(s);
                i += 1;
            }
            quiet.advance(VisualizerMode::Terrain, 1. / 60.);
            loud.advance(VisualizerMode::Terrain, 1. / 60.);
        }
        assert!(
            loud.energy() > quiet.energy() * 1.5,
            "gain did not reach the bands: {} vs {}",
            loud.energy(),
            quiet.energy()
        );
    }

    #[test]
    fn a_shorter_hold_lets_auto_switch_sooner() {
        let mut sw = OnsetSwitcher::new();
        sw.tune(1., 3.);
        // Calm for longer than the tuned hold but well under MIN_HOLD.
        assert!(!feed_calm(&mut sw, 4.));
        assert!(
            feed_hit(&mut sw, 0.5, 0.9),
            "no switch after the tuned hold"
        );
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
