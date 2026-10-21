//! The welcome screen's brain: points on the cortical surface of a real
//! brain, drawn as a slowly turning point cloud (`shaders/welcome.wgsl`).
//!
//! The points come from the MNI ICBM152 2009c nonlinear asymmetric template
//! (the standard fMRI template; McConnell Brain Imaging Centre, Montreal
//! Neurological Institute; notice in `brain/LICENSE-MNI152.txt`), via
//! TemplateFlow's 1 mm gray- and white-matter probability maps. The surface
//! is where gray + white matter crosses 0.5, reached from outside the brain,
//! so it runs down into the sulci. Each point has its outward normal and a
//! relief value, 1 on a gyral crown and 0 deep in a sulcus. The generator is
//! the ignored test `build_points_from_mni152` below.
//!
//! Units are about half the brain's front-to-back length; x is left to right,
//! y up, z toward the front.

/// Records of 10 bytes: x, y, z as i16 (units × 16000), the normal as 3 × i8
/// (× 127), and the relief as u8 (× 255).
const POINTS: &[u8] = include_bytes!("brain/mni152_points.bin");
const RECORD: usize = 10;
const POSITION_SCALE: f32 = 16000.0;

/// Every surface point: `[x, y, z, seed, nx, ny, nz, relief]`. `seed` (0-1)
/// varies each point's activity.
pub fn surface_points() -> Vec<[f32; 8]> {
    POINTS
        .chunks_exact(RECORD)
        .enumerate()
        .map(|(i, r)| {
            let c = |k: usize| i16::from_le_bytes([r[2 * k], r[2 * k + 1]]) as f32 / POSITION_SCALE;
            let n = |k: usize| r[6 + k] as i8 as f32 / 127.0;
            let seed = (i as u32).wrapping_mul(0x9e37_79b9).rotate_left(13).wrapping_mul(0x85eb_ca6b) as f32
                / u32::MAX as f32;
            [c(0), c(1), c(2), seed, n(0), n(1), n(2), r[9] as f32 / 255.0]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_are_a_brain_sized_surface() {
        let points = surface_points();
        assert!(points.len() >= 40_000, "{}", points.len());
        let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
        for p in &points {
            for k in 0..3 {
                lo[k] = lo[k].min(p[k]);
                hi[k] = hi[k].max(p[k]);
            }
            let n = (p[4] * p[4] + p[5] * p[5] + p[6] * p[6]).sqrt();
            assert!((n - 1.0).abs() < 0.03, "normal length {n}");
            assert!((0.0..=1.0).contains(&p[3]) && (0.0..=1.0).contains(&p[7]));
        }
        // Longest front to back, narrower side to side, both hemispheres, centered.
        let size = [hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]];
        assert!((1.9..2.1).contains(&size[2]), "length {size:?}");
        assert!(size[0] < 0.9 * size[2] && size[1] < 0.9 * size[2], "{size:?}");
        assert!(lo[0] < -0.6 && hi[0] > 0.6, "{lo:?} {hi:?}");
        assert!((lo[2] + hi[2]).abs() < 0.05, "centered front to back");
    }

    /// Rebuild `brain/mni152_points.bin` from TemplateFlow's
    /// `tpl-MNI152NLin2009cAsym_res-01_label-{GM,WM}_probseg.nii` (gunzipped)
    /// in `$ONSET_MNI152_DIR`:
    /// `ONSET_MNI152_DIR=... cargo test --lib build_points_from_mni152 -- --ignored`
    #[test]
    #[ignore]
    fn build_points_from_mni152() {
        let dir = std::path::PathBuf::from(std::env::var("ONSET_MNI152_DIR").expect("set ONSET_MNI152_DIR"));
        let name =
            |tissue: &str| dir.join(format!("tpl-MNI152NLin2009cAsym_res-01_label-{tissue}_probseg.nii"));
        let (dims, gm, srow) = read_nifti(&name("GM"));
        let (_, wm, _) = read_nifti(&name("WM"));
        let tissue: Vec<f32> = gm.iter().zip(&wm).map(|(g, w)| g + w).collect();
        let [nx, ny, nz] = dims;
        let at = |x: usize, y: usize, z: usize| x + nx * (y + ny * z);

        // Everything outside the brain, flooded in from the volume's edges,
        // down into the sulci.
        let mut outside = vec![false; tissue.len()];
        let mut stack = Vec::new();
        for z in 0..nz {
            for y in 0..ny {
                for x in 0..nx {
                    let edge = x == 0 || y == 0 || z == 0 || x == nx - 1 || y == ny - 1 || z == nz - 1;
                    if edge && tissue[at(x, y, z)] < 0.5 {
                        outside[at(x, y, z)] = true;
                        stack.push((x, y, z));
                    }
                }
            }
        }
        while let Some((x, y, z)) = stack.pop() {
            for (dx, dy, dz) in STEPS {
                let (u, v, w) = (x as i64 + dx, y as i64 + dy, z as i64 + dz);
                if u < 0 || v < 0 || w < 0 || u >= nx as i64 || v >= ny as i64 || w >= nz as i64 {
                    continue;
                }
                let i = at(u as usize, v as usize, w as usize);
                if !outside[i] && tissue[i] < 0.5 {
                    outside[i] = true;
                    stack.push((u as usize, v as usize, w as usize));
                }
            }
        }

        // Surface samples: where a brain voxel meets an outside voxel, the
        // point between them where tissue crosses 0.5.
        let mut samples = Vec::new();
        for z in 1..nz - 1 {
            for y in 1..ny - 1 {
                for x in 1..nx - 1 {
                    let t = tissue[at(x, y, z)];
                    if t < 0.5 {
                        continue;
                    }
                    for (dx, dy, dz) in STEPS {
                        let (u, v, w) =
                            ((x as i64 + dx) as usize, (y as i64 + dy) as usize, (z as i64 + dz) as usize);
                        if outside[at(u, v, w)] {
                            let f = (t - 0.5) / (t - tissue[at(u, v, w)]).max(1e-6);
                            samples.push([
                                x as f32 + dx as f32 * f,
                                y as f32 + dy as f32 * f,
                                z as f32 + dz as f32 * f,
                            ]);
                        }
                    }
                }
            }
        }
        // Relief: how buried a point is, from tissue blurred over a few mm
        // (more tissue around it means deeper in a sulcus).
        let blurred = blur(&tissue, dims, 2);
        let mut depth: Vec<f32> = samples.iter().map(|p| sample(&blurred, dims, *p)).collect();
        let mut sorted = depth.clone();
        sorted.sort_by(f32::total_cmp);
        let (lo, hi) = (sorted[sorted.len() / 20], sorted[sorted.len() * 19 / 20]);
        for d in &mut depth {
            *d = 1.0 - ((*d - lo) / (hi - lo)).clamp(0.0, 1.0);
        }
        // Keep 50,000, more of them on the crowns than down in the sulci,
        // so the folds show in the density too.
        let mut rng = crate::rng::Rng::new(152);
        let mut order: Vec<usize> = (0..samples.len()).collect();
        rng.shuffle(&mut order);
        let keep: Vec<usize> = order
            .into_iter()
            .filter(|&i| (rng.unit() as f32) < 0.12 + 0.88 * depth[i] * depth[i])
            .take(50_000)
            .collect();
        let samples: Vec<[f32; 3]> = keep.iter().map(|&i| samples[i]).collect();
        let depth: Vec<f32> = keep.iter().map(|&i| depth[i]).collect();

        // Voxel -> MNI mm (x right, y front, z up) -> ours (x right, y up,
        // z front), centered, half the front-to-back length = 1.
        let mm = |p: [f32; 3]| {
            let r = |row: [f32; 4]| row[0] * p[0] + row[1] * p[1] + row[2] * p[2] + row[3];
            [r(srow[0]), r(srow[2]), r(srow[1])]
        };
        let world: Vec<[f32; 3]> = samples.iter().map(|p| mm(*p)).collect();
        let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
        for p in &world {
            for k in 0..3 {
                lo[k] = lo[k].min(p[k]);
                hi[k] = hi[k].max(p[k]);
            }
        }
        let center = [(lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0, (lo[2] + hi[2]) / 2.0];
        let half = (hi[2] - lo[2]) / 2.0;

        let mut out = Vec::with_capacity(samples.len() * RECORD);
        for ((p, w), relief) in samples.iter().zip(&world).zip(&depth) {
            // Outward normal: down the tissue gradient (voxel axes are MNI axes).
            let g = gradient(&tissue, dims, *p);
            let g = [-g[0], -g[2], -g[1]];
            let l = (g[0] * g[0] + g[1] * g[1] + g[2] * g[2]).sqrt().max(1e-6);
            for k in 0..3 {
                let v = (w[k] - center[k]) / half;
                out.extend_from_slice(&((v * POSITION_SCALE).round() as i16).to_le_bytes());
            }
            for c in g {
                out.push(((c / l) * 127.0).round() as i8 as u8);
            }
            out.push((relief * 255.0).round() as u8);
        }
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/brain/mni152_points.bin");
        std::fs::write(&path, out).unwrap();
        println!(
            "{} points, {:?} mm, -> {}",
            samples.len(),
            [hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]],
            path.display()
        );
    }

    const STEPS: [(i64, i64, i64); 6] = [(1, 0, 0), (-1, 0, 0), (0, 1, 0), (0, -1, 0), (0, 0, 1), (0, 0, -1)];

    /// A NIfTI-1 float32 volume: its size, voxels (x fastest) and voxel-to-mm rows.
    fn read_nifti(path: &std::path::Path) -> ([usize; 3], Vec<f32>, [[f32; 4]; 3]) {
        let b = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let i16_at = |o: usize| i16::from_le_bytes([b[o], b[o + 1]]);
        let f32_at = |o: usize| f32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        assert_eq!(i32::from_le_bytes([b[0], b[1], b[2], b[3]]), 348, "NIfTI-1, little-endian");
        assert_eq!(i16_at(70), 16, "float32 voxels");
        let dims = [i16_at(42) as usize, i16_at(44) as usize, i16_at(46) as usize];
        let offset = f32_at(108) as usize;
        let slope = if f32_at(112) == 0.0 { 1.0 } else { f32_at(112) };
        let row = |o: usize| [f32_at(o), f32_at(o + 4), f32_at(o + 8), f32_at(o + 12)];
        let n = dims[0] * dims[1] * dims[2];
        let data = (0..n).map(|i| f32_at(offset + 4 * i) * slope + f32_at(116)).collect();
        (dims, data, [row(280), row(296), row(312)])
    }

    /// Box blur of radius `r` along each axis, twice (close to a Gaussian).
    fn blur(v: &[f32], [nx, ny, nz]: [usize; 3], r: usize) -> Vec<f32> {
        let mut a = v.to_vec();
        for _ in 0..2 {
            for (stride, len) in [(1, nx), (nx, ny), (nx * ny, nz)] {
                let lines: Vec<usize> = (0..a.len()).filter(|i| (i / stride) % len == 0).collect();
                let mut b = a.clone();
                for start in lines {
                    let mut sum = 0.0;
                    let get = |k: i64| a[start + k.clamp(0, len as i64 - 1) as usize * stride];
                    for k in -(r as i64)..=r as i64 {
                        sum += get(k);
                    }
                    for k in 0..len as i64 {
                        b[start + k as usize * stride] = sum / (2 * r + 1) as f32;
                        sum += get(k + r as i64 + 1) - get(k - r as i64);
                    }
                }
                a = b;
            }
        }
        a
    }

    /// Trilinear sample at voxel coordinates `p`.
    fn sample(v: &[f32], [nx, ny, nz]: [usize; 3], p: [f32; 3]) -> f32 {
        let clamp = |x: f32, n: usize| x.clamp(0.0, n as f32 - 1.001);
        let (x, y, z) = (clamp(p[0], nx), clamp(p[1], ny), clamp(p[2], nz));
        let (x0, y0, z0) = (x.floor() as usize, y.floor() as usize, z.floor() as usize);
        let (fx, fy, fz) = (x - x0 as f32, y - y0 as f32, z - z0 as f32);
        let at = |x: usize, y: usize, z: usize| v[x + nx * (y + ny * z)];
        let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
        let plane = |z: usize| {
            lerp(
                lerp(at(x0, y0, z), at(x0 + 1, y0, z), fx),
                lerp(at(x0, y0 + 1, z), at(x0 + 1, y0 + 1, z), fx),
                fy,
            )
        };
        lerp(plane(z0), plane(z0 + 1), fz)
    }

    fn gradient(v: &[f32], dims: [usize; 3], p: [f32; 3]) -> [f32; 3] {
        let d = |k: usize| {
            let (mut a, mut b) = (p, p);
            a[k] += 0.7;
            b[k] -= 0.7;
            (sample(v, dims, a) - sample(v, dims, b)) / 1.4
        };
        [d(0), d(1), d(2)]
    }
}
