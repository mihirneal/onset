//! The welcome screen's brain: a real brain seen from its left side, the
//! front facing left, as a matrix of dots (`shaders/welcome.wgsl`).
//!
//! The shape is the cortical surface of the MNI ICBM152 2009c nonlinear
//! asymmetric template (the standard fMRI template; McConnell Brain Imaging
//! Centre, Montreal Neurological Institute; notice in
//! `brain/LICENSE-MNI152.txt`), from TemplateFlow's 1 mm gray- and
//! white-matter probability maps. One dot sits on each cell of a regular grid
//! inside the brain's outline, shaded by the relief of the cortex nearest the
//! viewer there: 1 over gyral crowns, 0 over sulci. The generator is the
//! ignored test `build_side_dots_from_mni152` below.

/// Grid spacing, in units of half the brain's front-to-back length.
pub const SPACING: f32 = 0.038;

/// Records of 3 bytes: the grid column and row as i8 (column 0 at the
/// middle, front = negative; row up), and the relief as u8 (× 255).
const DOTS: &[u8] = include_bytes!("brain/side_dots.bin");

/// Every dot: `[x, y, seed, relief]`, with x and y in units of half the
/// brain's length (front = negative x, up = positive y) and `seed` (0-1)
/// varying each dot's activity.
pub fn side_dots() -> Vec<[f32; 4]> {
    DOTS.chunks_exact(3)
        .enumerate()
        .map(|(i, r)| {
            let seed = (i as u32).wrapping_mul(0x9e37_79b9).rotate_left(13).wrapping_mul(0x85eb_ca6b) as f32
                / u32::MAX as f32;
            [r[0] as i8 as f32 * SPACING, r[1] as i8 as f32 * SPACING, seed, r[2] as f32 / 255.0]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dots_fill_a_brain_shaped_outline() {
        let dots = side_dots();
        assert!((1000..4000).contains(&dots.len()), "{}", dots.len());
        let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
        for d in &dots {
            for k in 0..2 {
                lo[k] = lo[k].min(d[k]);
                hi[k] = hi[k].max(d[k]);
            }
            assert!((0.0..=1.0).contains(&d[2]) && (0.0..=1.0).contains(&d[3]));
        }
        // About two units front to back (centered), less from top to bottom.
        let size = [hi[0] - lo[0], hi[1] - lo[1]];
        assert!((1.85..2.05).contains(&size[0]) && (lo[0] + hi[0]).abs() < 0.1, "{lo:?} {hi:?}");
        assert!((1.2..1.9).contains(&size[1]), "{size:?}");
        // Crowns and sulci both show.
        assert!(dots.iter().any(|d| d[3] > 0.9) && dots.iter().any(|d| d[3] < 0.1));
    }

    /// Rebuild `brain/side_dots.bin` from TemplateFlow's
    /// `tpl-MNI152NLin2009cAsym_res-01_label-{GM,WM}_probseg.nii` (gunzipped)
    /// in `$ONSET_MNI152_DIR`:
    /// `ONSET_MNI152_DIR=... cargo test --lib build_side_dots_from_mni152 -- --ignored`
    #[test]
    #[ignore]
    fn build_side_dots_from_mni152() {
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

        // The cortical surface: where a brain voxel meets an outside voxel,
        // the point between them where tissue crosses 0.5. Relief: how
        // buried each point is, from tissue blurred over a few mm (more
        // tissue around means deeper in a sulcus).
        let blurred = blur(&tissue, dims, 2);
        let mut surface = Vec::new();
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
                            let p = [
                                x as f32 + dx as f32 * f,
                                y as f32 + dy as f32 * f,
                                z as f32 + dz as f32 * f,
                            ];
                            surface.push((p, sample(&blurred, dims, p)));
                        }
                    }
                }
            }
        }

        // Voxel -> MNI mm (x right, y front, z up), centered, scaled so half
        // the front-to-back length is 1.
        let mm = |p: [f32; 3]| {
            let r = |row: [f32; 4]| row[0] * p[0] + row[1] * p[1] + row[2] * p[2] + row[3];
            [r(srow[0]), r(srow[1]), r(srow[2])]
        };
        let world: Vec<[f32; 3]> = surface.iter().map(|(p, _)| mm(*p)).collect();
        let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
        for p in &world {
            for k in 0..3 {
                lo[k] = lo[k].min(p[k]);
                hi[k] = hi[k].max(p[k]);
            }
        }
        let center = [(lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0, (lo[2] + hi[2]) / 2.0];
        let half = (hi[1] - lo[1]) / 2.0;

        // Seen from the left: bin by (front to the left, up); in each cell,
        // average the relief of the surface nearest the viewer (smallest x).
        let mut cells: std::collections::BTreeMap<(i32, i32), Vec<[f32; 2]>> = Default::default();
        for (w, (_, buried)) in world.iter().zip(&surface) {
            let n = [(w[0] - center[0]) / half, (w[1] - center[1]) / half, (w[2] - center[2]) / half];
            let key = ((-n[1] / SPACING).round() as i32, (n[2] / SPACING).round() as i32);
            cells.entry(key).or_default().push([n[0], *buried]);
        }
        let mut dots: Vec<(i32, i32, f32)> = cells
            .into_iter()
            .map(|(key, pts)| {
                let near = pts.iter().map(|p| p[0]).fold(f32::MAX, f32::min);
                let front: Vec<f32> = pts.iter().filter(|p| p[0] <= near + 0.05).map(|p| p[1]).collect();
                (key.0, key.1, front.iter().sum::<f32>() / front.len() as f32)
            })
            .collect();
        // Less buried = higher relief, stretched so the folds still show
        // although a cell averages crowns and sulci.
        let mut sorted: Vec<f32> = dots.iter().map(|d| d.2).collect();
        sorted.sort_by(f32::total_cmp);
        let (lo, hi) = (sorted[sorted.len() / 10], sorted[sorted.len() * 9 / 10]);
        let mut out = Vec::with_capacity(dots.len() * 3);
        for (cx, cy, buried) in &mut dots {
            let relief = 1.0 - ((*buried - lo) / (hi - lo)).clamp(0.0, 1.0);
            out.extend([*cx as i8 as u8, *cy as i8 as u8, (relief * 255.0).round() as u8]);
        }
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/brain/side_dots.bin");
        std::fs::write(&path, out).unwrap();
        println!("{} dots from {} surface points -> {}", dots.len(), surface.len(), path.display());
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
}
