use std::io::prelude::*;
use std::fs::File;
use std::io::BufWriter;
use std::vec::Vec;
use std::env;
use std::convert::TryInto;
use std::f32;
use byteorder::{LittleEndian, WriteBytesExt};
use gltf;
use gltf::mesh::{Mesh, Primitive};

mod import;
use import::BufferData;

// File signature
const V3M_SIGNATURE: u32 = 0x52463344; // RF3D
//const V3C_SIGNATURE: u32 = 0x5246434D; // RFCM

// Supported format version
const V3D_VERSION: u32 = 0x40000;

// Section types
const V3D_END: u32       = 0x00000000; // terminating section
const V3D_SUBMESH: u32   = 0x5355424D;

type Vector3 = [f32; 3];
type Plane = [f32; 4];
type Matrix4 = [[f32; 4]; 4];
type Matrix3 = [[f32; 3]; 3];

fn create_custom_error<S: Into<String>>(msg: S) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, msg.into())
}

fn get_submesh_nodes(doc: &gltf::Document) -> impl Iterator<Item = gltf::Node> {
    doc.nodes().filter(|n| n.mesh().is_some())
}

fn get_submesh_textures(node: &gltf::Node) -> Vec<String> {
    let mesh = node.mesh().unwrap();
    let mut textures = mesh.primitives()
        .map(|prim| get_material_base_color_texture_name(&prim.material()))
        .collect::<Vec<_>>();
    textures.sort();
    textures.dedup();
    textures
}

fn write_v3d_header<W: Write>(wrt: &mut W, doc: &gltf::Document) -> std::io::Result<()> {
    let num_all_materials: usize = get_submesh_nodes(doc).map(|n| get_submesh_textures(&n).len()).sum();
    wrt.write_u32::<LittleEndian>(V3M_SIGNATURE)?;
    wrt.write_u32::<LittleEndian>(V3D_VERSION)?;
    let submesh_count = get_submesh_nodes(doc).count();
    wrt.write_u32::<LittleEndian>(submesh_count as u32)?;
    wrt.write_u32::<LittleEndian>(0)?; // num_all_vertices (unused by the game)
    wrt.write_u32::<LittleEndian>(0)?; // num_all_triangles (unused by the game)
    wrt.write_u32::<LittleEndian>(0)?; // unknown0
    wrt.write_u32::<LittleEndian>(num_all_materials as u32)?;
    wrt.write_u32::<LittleEndian>(0)?; // unknown1
    wrt.write_u32::<LittleEndian>(0)?; // unknown2
    wrt.write_u32::<LittleEndian>(0)?; // num_colspheres
    Ok(())
}

fn write_f32_slice<W: Write>(wrt: &mut W, slice: &[f32]) -> std::io::Result<()> {
    for i in 0..slice.len() {
        wrt.write_f32::<LittleEndian>(slice[i])?;
    }
    Ok(())
}

fn get_primitive_vertex_count(prim: &Primitive) -> usize {
    prim.attributes().find(|p| p.0 == gltf::mesh::Semantic::Positions).map(|a| a.1.count()).unwrap_or(0)
}

fn count_mesh_vertices(mesh: &Mesh) -> usize {
    mesh.primitives()
        .map(|p| get_primitive_vertex_count(&p))
        .sum()
}

fn compute_mesh_aabb(mesh: &Mesh, buffers: &Vec<BufferData>, transform: &Matrix3) -> gltf::mesh::BoundingBox {
    // Note: primitive AABB from gltf cannot be used because vertices are being transformed
    if count_mesh_vertices(mesh) == 0 {
        // Mesh has no vertices so return empty AABB
        return gltf::mesh::BoundingBox {
            min: [0f32; 3],
            max: [0f32; 3],
        };
    }
    let mut aabb = gltf::mesh::BoundingBox {
        min: [f32::MAX; 3],
        max: [f32::MIN; 3],
    };
    // Calculate AABB manually using vertex position data
    for prim in mesh.primitives() {
        let reader = prim.reader(|buffer| Some(&buffers[buffer.index()]));
        if let Some(iter) = reader.read_positions() {
            for pos in iter {
                let tpos = transform_point(&pos, transform);
                for i in 0..3 {
                    aabb.min[i] = aabb.min[i].min(tpos[i]);
                    aabb.max[i] = aabb.max[i].max(tpos[i]);
                }
            }
        }
    }
    aabb
}

fn get_vector_len(vec: &Vector3) -> f32 {
    vec.iter().map(|v| v * v).sum::<f32>().sqrt()
}

fn compute_mesh_bounding_sphere_radius(mesh: &Mesh, buffers: &Vec<BufferData>, transform: &Matrix3) -> f32 {
    let mut radius = 0f32;
    for prim in mesh.primitives() {
        let reader = prim.reader(|buffer| Some(&buffers[buffer.index()]));
        if let Some(iter) = reader.read_positions() {
            for pos in iter {
                let tpos = transform_point(&pos, transform);
                let diff = [tpos[0], tpos[1], tpos[2]];
                let dist = get_vector_len(&diff);
                radius = radius.max(dist);
            }
        } else {
            panic!("mesh has no positions");
        }
    }
    radius
}

fn transform_vector(pt: &Vector3, t: &Matrix3) -> Vector3 {
    let (x, y, z) = (pt[0], pt[1], pt[2]);
    [
        x * t[0][0] + y * t[1][0] + z * t[2][0],
        x * t[0][1] + y * t[1][1] + z * t[2][1],
        x * t[0][2] + y * t[1][2] + z * t[2][2],
    ]
}

fn transform_point(pt: &Vector3, t: &Matrix3) -> Vector3 {
    // for transforms without translation it is the same as for vector
    transform_vector(pt, t)
}

fn transform_normal(n: &Vector3, t: &Matrix3) -> Vector3 {
    let tn = transform_vector(n, t);
    // normalize transformed vector
    let l = get_vector_len(&tn);
    [tn[0] / l, tn[1] / l, tn[2] / l]
}

fn extract_translation_from_matrix(transform: &Matrix4) -> (Vector3, Matrix3) {
    let mut translation = [0f32; 3];
    translation.copy_from_slice(&transform[3][0..3]);
    let mut rot_scale_mat = [[0f32; 3]; 3];
    rot_scale_mat[0].copy_from_slice(&transform[0][0..3]);
    rot_scale_mat[1].copy_from_slice(&transform[1][0..3]);
    rot_scale_mat[2].copy_from_slice(&transform[2][0..3]);
    (translation, rot_scale_mat)
}

fn write_v3d_bounding_sphere<W: Write>(wrt: &mut W, mesh: &Mesh, buffers: &Vec<BufferData>, origin: &Vector3,
                                       transform: &Matrix3) -> std::io::Result<()> {

    let radius = compute_mesh_bounding_sphere_radius(mesh, buffers, &transform);
    write_f32_slice(wrt, origin)?;
    wrt.write_f32::<LittleEndian>(radius)?;
    Ok(())
}

fn write_v3d_bounding_box<W: Write>(wrt: &mut W, mesh: &Mesh, buffers: &Vec<BufferData>, transform: &Matrix3) -> std::io::Result<()> {
    let aabb = compute_mesh_aabb(mesh, buffers, transform);
    write_f32_slice(wrt, &aabb.min)?;
    write_f32_slice(wrt, &aabb.max)?;
    Ok(())
}

fn write_v3d_batch_header<W: Write>(mut wrt: W, prim: &Primitive, textures: &Vec::<String>) -> std::io::Result<()> {
    // unused data before texture index (game overrides it with data from v3d_batch_info)
    let unused_0 = [0u8; 0x20];
    wrt.write_all(&unused_0)?;
    // write texture index in LOD model textures array
    let texture_name = get_material_base_color_texture_name(&prim.material());
    let texture_idx = textures.iter().position(|t| t == &texture_name).expect("find texture");
    wrt.write_i32::<LittleEndian>(texture_idx as i32)?;
    // unused data after texture index (game overrides it with data from v3d_batch_info)
    let unused_24 = [0u8; 0x38 - 0x24];
    wrt.write_all(&unused_24)?;
    Ok(())
}

fn write_v3d_mesh_data_padding(wrt: &mut Vec<u8>) -> std::io::Result<()> {
    while wrt.len() & 0xF != 0 {
        wrt.write_u8(0)?;
    }
    Ok(())
}

fn compute_triangle_normal(p0: &Vector3, p1: &Vector3, p2: &Vector3) -> Vector3 {
    let t0 = [p1[0] - p0[0], p1[1] - p0[1], p1[2] - p0[2]];
    let t1 = [p2[0] - p1[0], p2[1] - p1[1], p2[2] - p1[2]];

    let mut normal = [0f32; 3];
    normal[0] = (t0[1] * t1[2]) - (t0[2] * t1[1]);
    normal[1] = (t0[2] * t1[0]) - (t0[0] * t1[2]);
    normal[2] = (t0[0] * t1[1]) - (t0[1] * t1[0]);

    let len = get_vector_len(&normal);
    normal[0] /= len;
    normal[1] /= len;
    normal[2] /= len;

    return normal;
}

fn compute_triangle_plane(p0: &Vector3, p1: &Vector3, p2: &Vector3) -> Plane {
    let [a, b, c] = compute_triangle_normal(p0, p1, p2);
    let d = -(a * p0[0] + b * p0[1] + c * p0[2]);
    [a, b, c, d]
}

fn generate_uv(pos: &Vector3, n: &Vector3) -> [f32; 2] {
    if n[0].abs() >= n[1].abs().max(n[2].abs()) {
        // X is greatest - left or right side
        [pos[0] + pos[2] * n[0].signum(), pos[1]]
    } else if n[1].abs() >= n[0].abs().max(n[2].abs()) {
        // Y is greatest - top or bottom side
        [pos[0] * n[1].signum(), pos[2]]
    } else {
        // Z is greatest - front or back side
        [pos[2] + pos[0] * n[2].signum(), pos[1]]
    }
}

fn write_v3d_batch_data(mut wrt: &mut Vec<u8>, prim: &Primitive, buffers: &Vec<BufferData>,
    transform: &Matrix3) -> std::io::Result<()> {
    
    let reader = prim.reader(|buffer| Some(&buffers[buffer.index()]));

    let positions = reader.read_positions().unwrap().collect::<Vec::<_>>();
    for pos in &positions {
        //println!("pos {:?}", pos);
        let tpos = transform_point(&pos, transform);
        write_f32_slice(&mut wrt, &tpos)?;
    }
    write_v3d_mesh_data_padding(wrt)?;

    let normals = reader.read_normals().unwrap().collect::<Vec::<_>>();
    for normal in &normals {
        let tnormal = transform_normal(&normal, transform);
        write_f32_slice(&mut wrt, &tnormal)?;
    }
    write_v3d_mesh_data_padding(wrt)?;

    if let Some(iter) = reader.read_tex_coords(0) {
        for uv in iter.into_f32() {
            write_f32_slice(&mut wrt, &uv)?;
        }
    } else {
        // use positions as fallback
        for i in 0..positions.len() {
            let uv = generate_uv(&positions[i], &normals[i]);
            //println!("uv {:?}", uv);
            write_f32_slice(&mut wrt, &uv)?;
        }
    }
    write_v3d_mesh_data_padding(wrt)?;

    if let Some(iter) = reader.read_indices() {
        let indices = iter.into_u32().collect::<Vec::<_>>();
        assert!(indices.len() % 3 == 0, "number of indices is not a multiple of three: {}", indices.len());

        // write indices
        for tri in indices.chunks(3) {
            //println!("Triangle: {} {} {}", tri[0], tri[1], tri[2]);
            wrt.write_u16::<LittleEndian>(tri[0].try_into().unwrap())?;
            wrt.write_u16::<LittleEndian>(tri[1].try_into().unwrap())?;
            wrt.write_u16::<LittleEndian>(tri[2].try_into().unwrap())?;
            let tri_flags = if prim.material().double_sided() { 0x20 } else { 0 };
            wrt.write_u16::<LittleEndian>(tri_flags)?;
        }
        write_v3d_mesh_data_padding(wrt)?;

        // write triangle planes (used for backface culling)
        // if(v3d_submesh_lod::flags & 0x20)
        for tri in indices.chunks(3) {
            let p0 = transform_point(&positions[tri[0] as usize], &transform);
            let p1 = transform_point(&positions[tri[1] as usize], &transform);
            let p2 = transform_point(&positions[tri[2] as usize], &transform);
            let plane = compute_triangle_plane(&p0, &p1, &p2);
            write_f32_slice(&mut wrt, &plane)?;
        }
        write_v3d_mesh_data_padding(wrt)?;

    } else {
        panic!("mesh has no indices");
    }

    // same_pos_vertex_offsets
    let num_vertices = get_primitive_vertex_count(prim);
    for _i in 0..num_vertices {
        wrt.write_i16::<LittleEndian>(0)?;
    }
    write_v3d_mesh_data_padding(wrt)?;

    // if (v3d_batch_info::bone_links_size)
    for _i in 0..num_vertices {
        let bone_link = [0u8; 0x8];
        wrt.write_all(&bone_link)?;
    }
    write_v3d_mesh_data_padding(wrt)?;

    // if (v3d_submesh_lod::flags & 0x1)
    // {
    //     float [v3d_submesh_lod::unknown0 * 2];
    //     // padding to 0x10 (to data section begin)
    // }
    Ok(())
}

fn create_v3d_mesh_data(mesh: &Mesh, buffers: &Vec<BufferData>, transform: &Matrix3, textures: &Vec::<String>) -> std::io::Result<Vec<u8>> {
    let mut wrt = Vec::<u8>::new();
    for prim in mesh.primitives() {
        write_v3d_batch_header(&mut wrt, &prim, textures)?; // batch_info
    }
    // padding to 0x10 (to data section begin)
    write_v3d_mesh_data_padding(&mut wrt)?;
    for prim in mesh.primitives() {
        write_v3d_batch_data(&mut wrt, &prim, buffers, transform)?; // batch_info
    }
    // padding to 0x10 (to data section begin)
    write_v3d_mesh_data_padding(&mut wrt)?;
    // no prop points
    Ok(wrt)
}

#[allow(dead_code)]
enum TextureSource {
    None = 0,
    Wrap = 1,
    Clamp = 2,
    ClampNoFiltering = 3,
    // Other types are used with multi-texturing
}

#[allow(dead_code)]
enum ColorOp
{
    SelectArg0IgnoreCurrentColor = 0x0,
    SelectArg0 = 0x1,
    Mul = 0x2,
    Add = 0x3,
    Mul2x = 0x4,
}

#[allow(dead_code)]
enum AlphaOp
{
    SelArg2 = 0x0,
    SelArg1 = 0x1,
    SelArg1IgnoreCurrentColor = 0x2,
    Mul = 0x3,
}

#[allow(dead_code)]
enum AlphaBlend
{
    None = 0x0,
    AlphaAdditive = 0x1,
    SrcAlpha2 = 0x2,
    AlphaBlendAlpha = 0x3,
    SrcAlpha4 = 0x4,
    DestColor = 0x5,
    InvDestColor = 0x6,
    SwappedSrcDestColor = 0x7,
}

#[allow(dead_code)]
enum ZbufferType
{
    None = 0x0,
    Read = 0x1,
    ReadEqFunc = 0x2,
    Write = 0x3,
    Full = 0x4,
    FullAlphaTest = 0x5,
}

#[allow(dead_code)]
enum FogType
{
    Type0 = 0x0,
    Type1 = 0x1,
    Type2 = 0x2,
    ForceOff = 0x3,
}

fn compute_render_state_for_material(material: &gltf::material::Material) -> u32 {
    // for example 0x518C41: tex_src = 1, color_op = 2, alpha_op = 3, alpha_blend = 3, zbuffer_type = 5, fog = 0
    let mut tex_src = TextureSource::Wrap;
    if let Some(tex_info) = material.pbr_metallic_roughness().base_color_texture() {
        use gltf::texture::WrappingMode;
        let sampler = tex_info.texture().sampler();
        if sampler.wrap_t() != sampler.wrap_s() {
            eprintln!("Ignoring wrapT - wrapping mode must be the same for T and S");
        }
        if sampler.wrap_s() == WrappingMode::MirroredRepeat {
            eprintln!("MirroredRepeat wrapping mode is not supported");
        }

        tex_src = if sampler.wrap_s() == WrappingMode::ClampToEdge {
            TextureSource::Clamp
        } else {
            TextureSource::Wrap
        };
    }

    let color_op = ColorOp::Mul;
    let alpha_op = AlphaOp::Mul;

    use gltf::material::AlphaMode;
    let alpha_blend = match material.alpha_mode() {
        AlphaMode::Blend => AlphaBlend::AlphaBlendAlpha,
        _ => AlphaBlend::None,
    };
    let zbuffer_type = match material.alpha_mode() {
        AlphaMode::Opaque => ZbufferType::Full,
        _ => ZbufferType::FullAlphaTest,
    };
    let fog = FogType::Type0;
    let state = tex_src as u32 | ((color_op as u32) << 5) | ((alpha_op as u32) << 10) | ((alpha_blend as u32) << 15)
        | ((zbuffer_type as u32) << 20) | ((fog as u32) << 25);
    state
}

fn write_v3d_batch_info<W: Write>(wrt: &mut W, prim: &Primitive) -> std::io::Result<()> {
    
    if prim.mode() != gltf::mesh::Mode::Triangles {
        return Err(create_custom_error("only triangle list primitives are supported"));
    }
    if prim.indices().is_none() {
        return Err(create_custom_error("not indexed geometry is not supported"));
    }

    let index_count = prim.indices().unwrap().count();
    assert!(index_count % 3 == 0, "number of indices is not a multiple of three: {}", index_count);
    let tri_count = index_count / 3;
    let index_limit = 10000 - 768;
    if index_count > index_limit {
        return Err(create_custom_error(format!("primitive has too many indices: {} (limit {})", index_count, index_limit)));
    }

    let vertex_count = get_primitive_vertex_count(prim);
    let vertex_limit = 6000 - 768;
    if vertex_count > 6000 {
        return Err(create_custom_error(format!("primitive has too many vertices: {} (limit {})", vertex_count, vertex_limit)));
    }

    wrt.write_u16::<LittleEndian>(vertex_count.try_into().unwrap())?; // vertices_count
    wrt.write_u16::<LittleEndian>(tri_count.try_into().unwrap())?; // triangles_count
    wrt.write_u16::<LittleEndian>((vertex_count * 3 * 4).try_into().unwrap())?; // positions_size
    wrt.write_u16::<LittleEndian>((tri_count * 4 * 2).try_into().unwrap())?; // triangles_size
    wrt.write_u16::<LittleEndian>((vertex_count * 2).try_into().unwrap())?; // same_pos_vertex_offsets_size
    wrt.write_u16::<LittleEndian>((vertex_count * 2 * 4).try_into().unwrap())?; // bone_links_size
    wrt.write_u16::<LittleEndian>((vertex_count * 2 * 4).try_into().unwrap())?; // tex_coords_size
    wrt.write_u32::<LittleEndian>(compute_render_state_for_material(&prim.material()))?; // render_state
    Ok(())
}

fn write_v3d_lod_texture<W: Write>(wrt: &mut W, tex_name: &str, textures: &Vec::<String>) -> std::io::Result<()> {
    let id = textures.iter().position(|n| n == tex_name).unwrap();
    wrt.write_u8(id.try_into().unwrap())?; // id
    wrt.write_all(tex_name.as_bytes())?;
    wrt.write_u8(0)?;
    Ok(())
}

fn write_v3d_lod_model<W: Write>(wrt: &mut W, mesh: &Mesh, buffers: &Vec<BufferData>, textures: &Vec::<String>,
    transform: &Matrix3) -> std::io::Result<()> {

    wrt.write_u32::<LittleEndian>(0x20)?; // flags, 0x1|0x02 - characters, 0x20 - static meshes, 0x10 only driller01.v3m
    wrt.write_u32::<LittleEndian>(count_mesh_vertices(mesh) as u32)?; // unknown0
    wrt.write_u16::<LittleEndian>(mesh.primitives().len() as u16)?; // num_batches

    let lod_textures = mesh.primitives().map(|prim| get_material_base_color_texture_name(&prim.material())).collect::<Vec<_>>();

    let batch_data = create_v3d_mesh_data(mesh, buffers, transform, &lod_textures)?;
    wrt.write_u32::<LittleEndian>(batch_data.len() as u32)?; // data_size
    wrt.write_all(&batch_data)?;

    wrt.write_i32::<LittleEndian>(-1)?; // unknown1
    for prim in mesh.primitives() {
        write_v3d_batch_info(wrt, &prim)?; // batch_info
    }

    wrt.write_u32::<LittleEndian>(0)?; // num_prop_points

    const MAX_TEXTURES: usize = 7;
    if lod_textures.len() > MAX_TEXTURES {
        return Err(create_custom_error(format!("found {} textures in a submesh but only {} are allowed",
            lod_textures.len(), MAX_TEXTURES)));
    }
    wrt.write_u32::<LittleEndian>(lod_textures.len() as u32)?;
    for tex_name in lod_textures {
        write_v3d_lod_texture(wrt, &tex_name, textures)?;
    }

    Ok(())
}

fn write_char_array<W: Write>(wrt: &mut W, string: &str, size: usize) -> std::io::Result<()> {
    let bytes = string.as_bytes();
    if bytes.len() >= size {
        return Err(create_custom_error(format!("string value {} is too long (max {})", string, size - 1)));
    }
    wrt.write_all(bytes)?;
    let padding = vec![0u8; size - bytes.len()];
    wrt.write_all(&padding)?;
    Ok(())
}

fn write_v3d_material<W: Write>(wrt: &mut W, tex_name: &str, emissive_factor: f32) -> std::io::Result<()> {
    write_char_array(wrt, tex_name, 32)?;
    wrt.write_f32::<LittleEndian>(emissive_factor)?;
    wrt.write_f32::<LittleEndian>(0.0)?; // unknown[0]
    wrt.write_f32::<LittleEndian>(0.0)?; // unknown[1]
    wrt.write_f32::<LittleEndian>(0.0)?; // ref_cof

    let ref_map_name_buf = [0u8; 32];
    wrt.write_all(&ref_map_name_buf)?;

    wrt.write_u32::<LittleEndian>(0x11)?; // flags
    Ok(())
}

fn change_texture_ext_to_tga(name: &str) -> String {
    let dot_offset = name.find('.').unwrap_or(name.len());
    let mut owned = name.to_owned();
    owned.replace_range(dot_offset.., ".tga");
    owned
}

fn get_material_base_color_texture_name(material: &gltf::material::Material) -> String {
    if let Some(tex_info) = material.pbr_metallic_roughness().base_color_texture() {
        let tex = tex_info.texture();
        let img = tex.source();
        if let Some(img_name) = img.name() {
            return change_texture_ext_to_tga(img_name);
        }
        if let gltf::image::Source::Uri { uri, .. } = img.source() {
            return change_texture_ext_to_tga(uri);
        }
    }
    const DEFAULT_TEXTURE: &'static str = "Rck_Default.tga";
    eprintln!("Cannot obtain texture name for material {} (materials without base color texture are not supported)",
        material.index().unwrap_or(0));
    DEFAULT_TEXTURE.into()
}

fn get_emissive_factor(mesh: &gltf::Mesh, texture: &str) -> f32 {
    mesh.primitives()
        .filter(|prim| get_material_base_color_texture_name(&prim.material()) == texture)
        .map(|prim| prim.material().emissive_factor())
        .map(|emissive_factor_rgb| emissive_factor_rgb.iter().cloned().fold(0f32, f32::max))
        .fold(0f32, f32::max)
}

fn write_v3d_subm_sect<W: Write>(wrt: &mut W, node: &gltf::Node, buffers: &Vec<BufferData>) -> std::io::Result<()> {
    wrt.write_u32::<LittleEndian>(V3D_SUBMESH)?; // section_type
    wrt.write_u32::<LittleEndian>(0)?; // section_size (ccrunch sets it to 0)

    let node_transform = node.transform().matrix();
    let mesh = node.mesh().unwrap();

    let name = node.name().unwrap_or("Default");
    write_char_array(wrt, name, 24)?;

    write_char_array(wrt, "None", 24)?; // unknown0
    wrt.write_u32::<LittleEndian>(7)?; // version
    wrt.write_u32::<LittleEndian>(1)?; // num_lods
    wrt.write_f32::<LittleEndian>(0.0)?; // lod_distances

    let (origin, rot_scale_mat) = extract_translation_from_matrix(&node_transform);
    write_v3d_bounding_sphere(wrt, &mesh, buffers, &origin, &rot_scale_mat)?;
    write_v3d_bounding_box(wrt, &mesh, buffers, &rot_scale_mat)?;

    let textures = get_submesh_textures(node);

    write_v3d_lod_model(wrt, &mesh, buffers, &textures, &rot_scale_mat)?;

    wrt.write_u32::<LittleEndian>(textures.len() as u32)?; // num_materials
    for tex_name in textures {
        let emissive_factor = get_emissive_factor(&mesh, &tex_name);
        write_v3d_material(wrt, &tex_name, emissive_factor)?;
    }

    wrt.write_u32::<LittleEndian>(1)?; // num_unknown1
    write_char_array(wrt, name, 24)?; // unknown1[0].unknown0
    wrt.write_f32::<LittleEndian>(0.0)?; // unknown1[0].unknown1

    Ok(())
}

fn write_v3d_end_sect<W: Write>(wrt: &mut W) -> std::io::Result<()> {
    wrt.write_u32::<LittleEndian>(V3D_END)?; // section_type
    wrt.write_u32::<LittleEndian>(0)?; // section_size (unused by the game)
    Ok(())
}

fn write_v3d<W: Write>(wrt: &mut W, document: &gltf::Document, buffers: &Vec<BufferData>) -> std::io::Result<()> {

    if document.nodes().filter(|n| n.children().count() > 0).count() > 0 {
        eprintln!("Node hierarchy is ignored!");
    }

    write_v3d_header(wrt, document)?;
    for node in get_submesh_nodes(document) {
        write_v3d_subm_sect(wrt, &node, buffers)?;
    }
    write_v3d_end_sect(wrt)?;
    Ok(())
}

fn convert_v3d(input_file_name: &str, output_file_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    println!("Importing GLTF file {}...", input_file_name);
    let gltf = gltf::Gltf::open(&input_file_name)?;
    let input_path: &std::path::Path = input_file_name.as_ref();
    let gltf::Gltf { document, blob } = gltf;

    println!("Importing GLTF buffers...");
    let buffers = import::import_buffer_data(&document, input_path.parent(), blob)?;
    
    println!("Converting...");
    let file = File::create(output_file_name)?;
    let mut wrt = BufWriter::new(file);
    write_v3d(&mut wrt, &document, &buffers)?;
    
    println!("Converted successfully.");
    Ok(())
}

fn main() {

    println!("GLTF to V3D converter 0.1 by Rafalh");

    let mut args = env::args();
    let app_name = args.next().unwrap();
    if env::args().len() != 3 {
        println!("Usage: {} input_file_name.gltf output_file_name.v3m", app_name);
        std::process::exit(1);
    }

    let input_file_name = args.next().unwrap();
    let output_file_name = args.next().unwrap();

    if let Err(e) = convert_v3d(&input_file_name, &output_file_name) {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
