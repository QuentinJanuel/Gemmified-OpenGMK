//! Game rendering functionality

pub mod atlas;
#[cfg(not(target_arch = "wasm32"))]
pub mod opengl;
#[cfg(target_arch = "wasm32")]
pub mod webgl;

use crate::{
    types::Colour,
    window::{connection::Connection, window::Window},
};
use atlas::{AtlasRect, AtlasRef};
use serde::{Deserialize, Serialize};
use std::any::Any;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum Scaling {
    /// Fixed scale, with a multiplier. The multiplier must always be strictly positive.
    Fixed(f64),
    /// Scale with window, but preserve aspect ratio.
    /// The f64 must be strictly negative and has no meaning, but can still be accessed with window_get_region_scale().
    Aspect(f64),
    /// Scale to fill window.
    Full,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedTexture {
    width: i32,
    height: i32,
    pixels: Box<[u8]>,
    zbuf: Option<Box<[f32]>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fog {
    pub colour: i32,
    pub begin: f32,
    pub end: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Light {
    Directional { direction: [f32; 3], colour: i32 },
    Point { position: [f32; 3], range: f32, colour: i32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlendType {
    Zero,
    One,
    SrcColour,
    InvSrcColour,
    SrcAlpha,
    InvSrcAlpha,
    DestAlpha,
    InvDestAlpha,
    DestColour,
    InvDestColour,
    SrcAlphaSaturate,
}

/// How a fragment's depth is compared against the z-buffer, as `d3d_set_zcomp_mode` selects it.
/// Stock GM8 never changes it -- only the GM8.2 extension can -- so until one does, the context
/// keeps whichever comparison it starts with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DepthFunc {
    Never,
    Less,
    Equal,
    LessEqual,
    Greater,
    NotEqual,
    GreaterEqual,
    Always,
}

impl From<i32> for DepthFunc {
    /// The numbers are D3D9's `D3DCMP_*`, which is what a game passes: GM8.2's `cm_*` constants
    /// are those values verbatim. Anything else keeps the default rather than inventing a mode.
    fn from(cmp: i32) -> Self {
        match cmp {
            1 => DepthFunc::Never,
            2 => DepthFunc::Less,
            3 => DepthFunc::Equal,
            5 => DepthFunc::Greater,
            6 => DepthFunc::NotEqual,
            7 => DepthFunc::GreaterEqual,
            8 => DepthFunc::Always,
            _ => DepthFunc::LessEqual,
        }
    }
}

/// Where the fixed-function pipeline gets stage 0's texture coordinates, as GM8.2's
/// `texture_set_mode` selects it. Stock GM8 has no such function, so a vertex's own coordinates are
/// the only source until the extension asks for another.
///
/// This matters more than a texture-coordinate tweak usually would: a model saved with
/// `d3d_model_save` stores *no* texture coordinates for a `vertex_normal` command, so a game
/// texturing one this way has nothing else to sample by. renex engine's teapot boss is exactly that
/// -- 7086 normals, every UV zero, and `tm_spheremap` supplying all of them.
///
/// The discriminants are what the vertex shader switches on, so this goes into the uniform block
/// as-is. They are deliberately *not* D3D's `D3DTSS_TCI_*` values -- see `from_d3d_tci`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(i32)]
pub enum TexcoordSource {
    /// `tm_regular` -- the vertex's own texture coordinates.
    Vertex = 0,
    /// `tm_csnormal` -- the camera-space vertex normal.
    CameraSpaceNormal = 1,
    /// `tm_csposition` -- the camera-space vertex position.
    CameraSpacePosition = 2,
    /// `tm_csreflect` -- the camera-space reflection vector.
    CameraSpaceReflect = 3,
    /// `tm_spheremap` -- sphere-map coordinates derived from the camera-space normal.
    SphereMap = 4,
}

impl TexcoordSource {
    /// Reads D3D9's `D3DTSS_TEXCOORDINDEX`, which is what a game passes: GM8.2's `tm_*` constants
    /// are the `D3DTSS_TCI_*` values verbatim, and `texture_set_mode` is a bare
    /// `SetTextureStageState(0, D3DTSS_TEXCOORDINDEX, mode)`.
    ///
    /// The *low* half of that state picks which coordinate set a passthrough reads. This renderer's
    /// vertices carry exactly one, so it is masked off rather than misread as a mode -- which also
    /// makes `texture_set_mode(0)` and `texture_set_mode(1)` both mean "the vertex's own", as they
    /// do on D3D for a single-texcoord vertex format.
    ///
    /// An unknown generator is `Vertex` rather than a guess: drawing with the model's own
    /// coordinates is what the game had before it asked, and is never worse than inventing a mapping.
    pub fn from_d3d_tci(mode: i32) -> Self {
        match mode as u32 & 0xffff_0000 {
            0x0001_0000 => TexcoordSource::CameraSpaceNormal,
            0x0002_0000 => TexcoordSource::CameraSpacePosition,
            0x0003_0000 => TexcoordSource::CameraSpaceReflect,
            0x0004_0000 => TexcoordSource::SphereMap,
            _ => TexcoordSource::Vertex,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrimitiveType {
    PointList,
    LineList,
    LineStrip,
    TriList,
    TriStrip,
    TriFan,
}

impl From<i32> for PrimitiveType {
    fn from(pt: i32) -> Self {
        match pt {
            0 | 1 => PrimitiveType::PointList,
            2 => PrimitiveType::LineList,
            3 => PrimitiveType::LineStrip,
            4 => PrimitiveType::TriList,
            5 => PrimitiveType::TriStrip,
            6 => PrimitiveType::TriFan,
            _ => PrimitiveType::PointList, // GM8 just draws nothing in this case
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum PrimitiveShape {
    Point,
    Line,
    Triangle,
}

impl From<PrimitiveType> for PrimitiveShape {
    fn from(pt: PrimitiveType) -> Self {
        match pt {
            PrimitiveType::PointList => PrimitiveShape::Point,
            PrimitiveType::LineList | PrimitiveType::LineStrip => PrimitiveShape::Line,
            PrimitiveType::TriList | PrimitiveType::TriStrip | PrimitiveType::TriFan => PrimitiveShape::Triangle,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct Vertex {
    pub pos: [f32; 3],
    pub tex_coord: [f32; 2],
    pub blend: [f32; 4],
    pub atlas_xywh: [f32; 4],
    pub normal: [f32; 3],
}

/// A builder to be used for building primitives.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PrimitiveBuilder {
    vertices: Vec<Vertex>,
    ptype: PrimitiveType,
    atlas_ref: AtlasRect,
    textured: bool,
}

impl PrimitiveBuilder {
    fn new(atlas_ref: AtlasRect, ptype: PrimitiveType, textured: bool) -> Self {
        Self { vertices: Vec::new(), ptype, atlas_ref, textured }
    }

    fn push_vertex_raw(&mut self, v: Vertex) -> &mut Self {
        // if we need to fill out a shape get the other two points
        let (v1, v2) = match self.ptype {
            PrimitiveType::PointList | PrimitiveType::LineList | PrimitiveType::TriList => (None, None),
            PrimitiveType::LineStrip => (self.vertices.last().filter(|_| self.vertices.len() >= 2).copied(), None),
            PrimitiveType::TriFan | PrimitiveType::TriStrip if self.vertices.len() < 3 => (None, None),
            PrimitiveType::TriStrip | PrimitiveType::TriFan => {
                (Some(self.vertices[self.vertices.len() - 2]), self.vertices.last().copied())
            },
        };
        if let Some(v1) = v1 {
            self.vertices.push(v1);
        }
        self.vertices.push(v);
        if let Some(v2) = v2 {
            self.vertices.push(v2);
            let len = self.vertices.len();
            if len % 6 == 3 && self.ptype == PrimitiveType::TriStrip {
                self.vertices.swap(len - 2, len - 1);
                self.vertices.swap(len - 3, len - 2);
            }
        } else if self.vertices.len() == 3 && self.ptype == PrimitiveType::TriFan {
            // i hate that this works
            self.vertices.swap(0, 1);
            self.vertices.swap(1, 2);
        }
        self
    }

    /// Keeps texture coordinates even though this primitive has no texture.
    ///
    /// For a *drawn* primitive, discarding them is right: NaN is how the built-in program is told
    /// there is no texture. For one being baked into a model's cache it is destructive, because the
    /// cache outlives the draw that built it and the texture comes from each later draw -- and GM8.2
    /// builds every cache untextured, since `d3d_model_bake(m)` is defined as
    /// `d3d_model_draw(m,0,0,0,-1)`. Whether the draw has a texture is decided per draw instead; see
    /// `draw_buffers`.
    pub fn keep_tex_coords(&mut self) {
        self.textured = true;
    }

    fn push_vertex(&mut self, pos: [f32; 3], tex_coord: [f32; 2], blend: [f32; 4], normal: [f32; 3]) -> &mut Self {
        let tex_coord = if self.textured { tex_coord } else { [f32::NAN; 2] };
        self.push_vertex_raw(Vertex { pos, tex_coord, blend, normal, atlas_xywh: self.atlas_ref.into() });
        self
    }

    fn get_atlas_id(&self) -> u32 {
        self.atlas_ref.atlas_id
    }

    fn get_shape(&self) -> PrimitiveShape {
        self.ptype.into()
    }

    fn get_vertices(&self) -> &[Vertex] {
        &self.vertices
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VertexBuffer {
    points: Vec<Vertex>,
    lines: Vec<Vertex>,
    tris: Vec<Vertex>,
}

pub struct Renderer(
    Box<dyn RendererTrait>,
    /// Whether primitives must keep their texture coordinates even when begun with no texture --
    /// set while a model's vertex cache is being filled. See `keep_tex_coords`.
    bool,
);

pub trait RendererTrait {
    fn as_any(&self) -> &dyn Any;
    fn max_texture_size(&self) -> u32;
    fn push_atlases(&mut self, atl: atlas::AtlasBuilder) -> Result<(), String>;
    fn upload_sprite(
        &mut self,
        data: Box<[u8]>,
        width: i32,
        height: i32,
        origin_x: i32,
        origin_y: i32,
    ) -> Result<AtlasRef, String>;
    fn duplicate_sprite(&mut self, atlas_ref: AtlasRef) -> Result<AtlasRef, String>;
    fn delete_sprite(&mut self, atlas_ref: AtlasRef);

    /// Moves a sprite's drawing origin, which `upload_sprite` otherwise fixes for good at upload
    /// time. A sprite's origin is stored twice: here (normalized by the sprite's size, and used for
    /// *drawing*) and on the `Sprite` asset (in pixels, and used for *collision*). `sprite_set_offset`
    /// can move it at runtime, so it has to write through to both or the two disagree — a sprite
    /// drawn one place and collided somewhere else.
    fn set_sprite_origin(&mut self, atlas_ref: AtlasRef, origin_x: i32, origin_y: i32);

    /// Resizes the rendering target. Usually called when the window has been resized.
    /// The contents of the old framebuffer will be copied to the new one. If `store` is true, the old one
    /// will be stored for later drawing with `draw_stored()`; otherwise, it will be deleted.
    fn resize_framebuffer(&mut self, width: u32, height: u32, store: bool);

    fn set_vsync(&self, vsync: bool);
    fn get_vsync(&self) -> bool;
    fn wait_vsync(&self);

    fn get_rect(&self, id: AtlasRef) -> Option<&AtlasRect>;

    fn draw_sprite(&mut self, tex: AtlasRef, x: f64, y: f64, xs: f64, ys: f64, ang: f64, col: i32, alpha: f64) {
        if let Some(rect) = self.get_rect(tex) {
            let (w, h) = (rect.w, rect.h);
            self.draw_sprite_general(
                tex,
                0.0,
                0.0,
                w.into(),
                h.into(),
                x,
                y,
                xs,
                ys,
                ang,
                col,
                col,
                col,
                col,
                alpha,
                true,
            );
        }
    }

    fn draw_sprite_colour(
        &mut self,
        tex: AtlasRef,
        x: f64,
        y: f64,
        xs: f64,
        ys: f64,
        ang: f64,
        col1: i32,
        col2: i32,
        col3: i32,
        col4: i32,
        alpha: f64,
    ) {
        if let Some(rect) = self.get_rect(tex) {
            let (w, h) = (rect.w, rect.h);
            self.draw_sprite_general(
                tex,
                0.0,
                0.0,
                w.into(),
                h.into(),
                x,
                y,
                xs,
                ys,
                ang,
                col1,
                col2,
                col3,
                col4,
                alpha,
                true,
            );
        }
    }

    fn draw_sprite_general(
        &mut self,
        texture: AtlasRef,
        part_x: f64,
        part_y: f64,
        part_w: f64,
        part_h: f64,
        x: f64,
        y: f64,
        xscale: f64,
        yscale: f64,
        angle: f64,
        col1: i32,
        col2: i32,
        col3: i32,
        col4: i32,
        alpha: f64,
        use_origin: bool,
    );
    fn draw_sprite_pos(
        &mut self,
        texture: AtlasRef,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        x3: f64,
        y3: f64,
        x4: f64,
        y4: f64,
        alpha: f64,
    );
    fn set_view_matrix(&mut self, view: [f32; 16]);
    fn set_viewproj_matrix(&mut self, view: [f32; 16], proj: [f32; 16]);
    fn get_view_matrix(&self) -> [f32; 16];
    fn get_proj_matrix(&self) -> [f32; 16];
    fn get_model_matrix(&self) -> [f32; 16];
    fn set_model_matrix(&mut self, model: [f32; 16]);
    fn mult_model_matrix(&mut self, model: [f32; 16]);
    fn set_projection_ortho(&mut self, x: f64, y: f64, w: f64, h: f64, angle: f64);
    fn set_projection_perspective(&mut self, x: f64, y: f64, w: f64, h: f64, angle: f64);
    fn set_view(
        &mut self,
        src_x: i32,
        src_y: i32,
        src_w: i32,
        src_h: i32,
        src_angle: f64,
        port_x: i32,
        port_y: i32,
        port_w: i32,
        port_h: i32,
    );
    /// The part of the drawing buffer being rendered into, as (x, y, width, height). `set_view`
    /// sets this from the room's view port; `d3d_set_viewport` overrides it directly.
    fn get_viewport(&self) -> (i32, i32, i32, i32);
    fn set_viewport(&mut self, x: i32, y: i32, w: i32, h: i32);
    fn flush_queue(&mut self);
    fn present(&mut self, window_width: u32, window_height: u32, scaling: Scaling);
    fn draw_stored(&mut self, target_x: i32, target_y: i32, width: u32, height: u32);
    fn stored_size(&self) -> (u32, u32);
    fn finish(&mut self, window_width: u32, window_height: u32, clear_colour: Colour);

    fn dump_sprite(&self, atlas_ref: AtlasRef) -> Box<[u8]> {
        match self.get_rect(atlas_ref) {
            Some(rect) => self.dump_sprite_part(atlas_ref, 0, 0, rect.w, rect.h),
            None => Box::new([]),
        }
    }
    fn dump_sprite_part(&self, texture: AtlasRef, part_x: i32, part_y: i32, part_w: i32, part_h: i32) -> Box<[u8]>;
    fn get_alpha_blending(&self) -> bool;
    fn set_alpha_blending(&mut self, alphablend: bool);
    fn get_colour_blending(&self) -> bool;
    fn set_colour_blending(&mut self, modulate: bool);
    fn get_blend_mode(&self) -> (BlendType, BlendType);
    fn set_blend_mode(&mut self, src: BlendType, dst: BlendType);
    /// The blend factors used for the alpha channel alone. `None` -- the state every stock GM8
    /// game stays in -- blends alpha with the same factors as colour.
    fn set_blend_mode_alpha(&mut self, modes: Option<(BlendType, BlendType)>);
    /// Which channels a draw is allowed to write. Used to punch alpha into a surface without
    /// disturbing its colour, and vice versa.
    fn set_colour_mask(&mut self, r: bool, g: bool, b: bool, a: bool);
    fn set_depth_func(&mut self, func: DepthFunc);
    /// A constant added to a fragment's depth before it is compared, in depth-buffer units, to
    /// keep coplanar polygons from z-fighting.
    fn set_depth_bias(&mut self, bias: f64);
    /// The depth range a drawn fragment's z is mapped into, both within 0..1.
    fn set_depth_range(&mut self, near: f64, far: f64);
    fn get_pixel_interpolation(&self) -> bool;
    fn set_pixel_interpolation(&mut self, lerping: bool);
    fn get_texture_repeat(&self) -> bool;
    fn set_texture_repeat(&mut self, repeat: bool);

    fn get_pixels(&self, x: i32, y: i32, w: i32, h: i32) -> Box<[u8]>;
    fn stored_pixels(&self) -> Box<[u8]>;
    fn stored_zbuffer(&self) -> Box<[f32]>;
    fn set_stored(&mut self, rgba: Box<[u8]>, zbuf: Box<[f32]>, fb_w: u32, fb_h: u32);

    fn dump_dynamic_textures(&self) -> Vec<Option<SavedTexture>>;
    fn upload_dynamic_textures(&mut self, textures: &[Option<SavedTexture>]);

    fn create_sprite_colour(&mut self, width: i32, height: i32, col: Colour) -> Result<AtlasRef, String>;
    fn create_surface(&mut self, w: i32, h: i32, has_zbuffer: bool) -> Result<AtlasRef, String>;
    fn set_target(&mut self, atlas_ref: AtlasRef);
    fn reset_target(&mut self);
    fn copy_surface(
        &mut self,
        dest: AtlasRef,
        dest_x: i32,
        dest_y: i32,
        src: AtlasRef,
        src_x: i32,
        src_y: i32,
        width: i32,
        height: i32,
    );

    fn set_zbuf_trashed(&mut self, trashed: bool);
    fn get_zbuf_trashed(&self) -> bool;

    fn get_texture_id(&mut self, atl_ref: AtlasRef) -> i32;
    fn get_texture_from_id(&self, id: i32) -> Option<AtlasRef>;

    fn get_texture_rects(&self) -> Vec<Option<AtlasRect>>;
    fn set_texture_rects(&mut self, rects: &[Option<AtlasRect>]);

    fn draw_sprite_partial(
        &mut self,
        texture: AtlasRef,
        part_x: f64,
        part_y: f64,
        part_w: f64,
        part_h: f64,
        x: f64,
        y: f64,
        xscale: f64,
        yscale: f64,
        angle: f64,
        colour: i32,
        alpha: f64,
    ) {
        self.draw_sprite_general(
            texture, part_x, part_y, part_w, part_h, x, y, xscale, yscale, angle, colour, colour, colour, colour,
            alpha, false,
        )
    }
    fn draw_sprite_tiled(
        &mut self,
        texture: AtlasRef,
        mut x: f64,
        mut y: f64,
        xscale: f64,
        yscale: f64,
        colour: i32,
        alpha: f64,
        tile_end_x: Option<f64>,
        tile_end_y: Option<f64>,
    ) {
        let (width, height) = match self.get_rect(texture) {
            Some(rect) => (f64::from(rect.w) * xscale, f64::from(rect.h) * yscale),
            None => return,
        };

        if tile_end_x.is_some() {
            x = x.rem_euclid(width);
            if x > 0.0 {
                x -= width;
            }
        }
        if tile_end_y.is_some() {
            y = y.rem_euclid(height);
            if y > 0.0 {
                y -= height;
            }
        }

        let start_x = x;

        loop {
            loop {
                self.draw_sprite(texture, x, y, xscale, yscale, 0.0, colour, alpha);
                x += width;
                match tile_end_x {
                    Some(end_x) if x < end_x => (),
                    _ => break,
                }
            }
            x = start_x;
            y += height;
            match tile_end_y {
                Some(end_y) if y < end_y => (),
                _ => break,
            }
        }
    }

    fn draw_rectangle(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, colour: i32, alpha: f64);
    fn draw_rectangle_outline(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, colour: i32, alpha: f64);
    fn draw_rectangle_gradient(
        &mut self,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        c1: i32,
        c2: i32,
        c3: i32,
        c4: i32,
        alpha: f64,
        outline: bool,
    );
    fn draw_point(&mut self, x: f64, y: f64, colour: i32, alpha: f64);
    fn draw_line(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, width: Option<f64>, c1: i32, c2: i32, alpha: f64);
    fn draw_triangle(
        &mut self,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        x3: f64,
        y3: f64,
        c1: i32,
        c2: i32,
        c3: i32,
        alpha: f64,
        outline: bool,
    );
    fn draw_ellipse(&mut self, x: f64, y: f64, rad_x: f64, rad_y: f64, c1: i32, c2: i32, alpha: f64, outline: bool);
    fn draw_roundrect(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, c1: i32, c2: i32, alpha: f64, outline: bool);
    fn set_circle_precision(&mut self, prec: i32);
    fn get_circle_precision(&self) -> i32;
    fn reset_primitive_2d(&mut self, ptype: PrimitiveType, atlas_ref: Option<AtlasRef>);
    fn vertex_2d(&mut self, x: f64, y: f64, xtex: f64, ytex: f64, col: i32, alpha: f64);
    fn draw_primitive_2d(&mut self);
    fn get_primitive_2d(&self) -> PrimitiveBuilder;
    fn set_primitive_2d(&mut self, prim: PrimitiveBuilder);
    fn reset_primitive_3d(&mut self, ptype: PrimitiveType, atlas_ref: Option<AtlasRef>);
    fn vertex_3d(
        &mut self,
        x: f64,
        y: f64,
        z: f64,
        nx: f64,
        ny: f64,
        nz: f64,
        xtex: f64,
        ytex: f64,
        col: i32,
        alpha: f64,
    );
    fn draw_primitive_3d(&mut self);
    fn get_primitive_3d(&self) -> PrimitiveBuilder;
    fn set_primitive_3d(&mut self, prim: PrimitiveBuilder);
    fn extend_buffers(&self, buf: &mut VertexBuffer);
    fn draw_buffers(&mut self, atlas_ref: Option<AtlasRef>, buf: &VertexBuffer);
    fn clear_view(&mut self, colour: Colour, alpha: f64);
    fn clear_view_no_zbuf(&mut self, colour: Colour, alpha: f64);
    fn clear_zbuf(&mut self);

    fn get_3d(&self) -> bool;
    fn set_3d(&mut self, use_3d: bool);
    fn get_depth(&self) -> f32;
    fn set_depth(&mut self, depth: f32);
    fn get_depth_test(&self) -> bool;
    fn set_depth_test(&mut self, depth_test: bool);
    fn get_write_depth(&self) -> bool;
    fn set_write_depth(&mut self, write_depth: bool);
    fn get_culling(&self) -> bool;
    fn set_culling(&mut self, culling: bool);
    /// Which winding culling throws away. `false` -- what stock GM8's `d3d_set_culling` means --
    /// culls counter-clockwise faces, matching D3D's `D3DCULL_CCW`.
    fn set_cull_clockwise(&mut self, clockwise: bool);
    fn get_perspective(&self) -> bool;
    fn set_perspective(&mut self, perspective: bool);
    fn get_fog(&self) -> Option<Fog>;
    fn set_fog(&mut self, fog: Option<Fog>);
    fn get_gouraud(&self) -> bool;
    fn set_gouraud(&mut self, gouraud: bool);
    fn get_lighting_enabled(&self) -> bool;
    fn set_lighting_enabled(&mut self, enabled: bool);
    fn get_ambient_colour(&self) -> i32;
    fn set_ambient_colour(&mut self, colour: i32);
    fn get_lights(&self) -> [(bool, Light); 8];
    fn set_lights(&mut self, lights: [(bool, Light); 8]);
    fn set_light_enabled(&mut self, id: usize, enabled: bool);
    fn set_light(&mut self, id: usize, light: Light);
    fn get_texcoord_source(&self) -> TexcoordSource;
    fn set_texcoord_source(&mut self, source: TexcoordSource);

    // -- game-supplied shaders (GM8.2's `shader_*` family) ---------------------------------------
    //
    // Every method here has an inert default, so a backend that has not implemented custom shaders
    // needs no changes at all and a game asking for one is told no. That is not politeness: this
    // file's trait is shared with `render/opengl.rs`, which is `#[cfg]`-ed out of the wasm build, so
    // a required method there would be a change the wasm build cannot check.

    /// Compiles one stage from GLSL. `Err` is the compile log, which is the only useful thing to
    /// report when a translated shader turns out not to be valid after all.
    fn create_shader(&mut self, _stage: ShaderStage, _glsl: &str) -> Result<ShaderRef, String> {
        Err("this renderer does not support custom shaders".into())
    }

    fn delete_shader(&mut self, _shader: ShaderRef) {}

    /// Sets, or with `None` clears, one stage. D3D9 binds the two independently; the pair is only
    /// resolved into a linked program at draw time.
    fn set_shader_stage(&mut self, _stage: ShaderStage, _shader: Option<ShaderRef>) {}

    fn get_shader_stage(&self, _stage: ShaderStage) -> Option<ShaderRef> {
        None
    }

    /// Writes one `c#` constant register. Registers rather than named uniforms because that is what
    /// GM8.2's API sets -- see `sm3`'s `glsl` module.
    fn set_shader_constant(&mut self, _stage: ShaderStage, _register: u32, _value: [f32; 4]) {}

    /// Binds a texture to sampler register `s#` of one stage. `None` leaves it reading whatever the
    /// draw itself supplies, which for pixel stage 0 is the sprite or surface being drawn -- the
    /// same thing D3D does when a game sets no texture of its own.
    ///
    /// The two stages have separate sampler registers in D3D9 (`texture_set_stage` against
    /// `texture_set_stage_vertex`), so `s0` means two different things depending on `kind`.
    fn set_shader_sampler(&mut self, _kind: ShaderStage, _stage: u32, _texture: Option<AtlasRef>) {}

    /// Per-stage sampler state: how a stage filters, and whether it wraps. `None` leaves whichever
    /// is not being set alone.
    ///
    /// Per *stage*, not per texture, which is the distinction D3D draws and GL only gained with
    /// sampler objects -- the same texture read through two stages can be filtered two ways.
    fn set_shader_sampler_state(
        &mut self,
        _kind: ShaderStage,
        _stage: u32,
        _interpolate: Option<bool>,
        _repeat: Option<(bool, bool)>,
    ) {
    }

    /// Draws a game-supplied vertex buffer through the shader it set, without decoding it.
    ///
    /// Returns `false` when there is no custom program to draw with, which is the caller's cue to
    /// fall back to unpacking the buffer into immediate-mode calls -- the fixed-function pipeline
    /// has no other way to read a game's own vertex declaration.
    fn draw_vertex_streams(
        &mut self,
        _streams: &[VertexStream],
        _indices: Option<IndexData>,
        _primitive: PrimitiveType,
        _vertex_count: usize,
        _instance_count: usize,
        _texture: Option<AtlasRef>,
    ) -> bool {
        false
    }

    /// The combined view-projection matrix *as the built-in program receives it*, including the
    /// vertical flip and half-pixel shift this renderer folds in for GL.
    ///
    /// A custom vertex shader writes `gl_Position` from a matrix the game handed it, so unless that
    /// matrix carries the same flip, everything drawn through the shader comes out mirrored against
    /// everything drawn without one. Handing back the renderer's own matrix is what keeps the two
    /// halves of a frame in the same space.
    fn get_viewproj_matrix(&self) -> [f32; 16] {
        let mut identity = [0.0; 16];
        for i in 0..4 {
            identity[i * 5] = 1.0;
        }
        identity
    }
}

/// One vertex stream of a game-supplied draw: the raw bytes, how far apart consecutive entries
/// are, and which declared elements are read out of it.
///
/// Several streams because D3D9's vertex declaration is per-stream and GM8.2 exposes that: the
/// shadow-volume pass in Delicious Level Design Contest puts one unit quad in stream 0 and a
/// per-light transform in stream 1, so `divisor` is what turns the second into instancing.
pub struct VertexStream<'a> {
    pub data: &'a [u8],
    pub stride: usize,
    /// `(vf_type, vf_usage, usage index, byte offset within the stride)`, in declaration order.
    ///
    /// The usage *index* matters as much as the usage: a declaration may carry two positions -- one
    /// per vertex and one per instance -- and `position0` and `position1` are what tells the shader
    /// which is which. D3D numbers them by order of appearance, so that is how they are counted.
    pub elements: &'a [(i32, i32, u8, usize)],
    /// 0 advances per vertex, 1 per instance.
    pub divisor: u32,
}

/// An index buffer: the raw bytes, and whether they are 32-bit rather than 16-bit.
pub struct IndexData<'a> {
    pub data: &'a [u8],
    pub wide: bool,
}

/// Which half of the pipeline a shader is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShaderStage {
    Vertex,
    Pixel,
}

/// A compiled shader stage. Opaque, and only meaningful to the renderer that made it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub struct ShaderRef(pub i32);

pub struct RendererOptions {
    pub size: (u32, u32),
    pub vsync: bool,
    pub interpolate_pixels: bool,
    pub normalize_normals: bool,
    pub zbuf_24: bool,
}

impl Default for RendererOptions {
    fn default() -> Self {
        RendererOptions {
            size: (8, 8),
            vsync: true,
            interpolate_pixels: false,
            normalize_normals: false,
            zbuf_24: false,
        }
    }
}

impl Renderer {
    pub fn new(
        backend: (),
        connection: &Connection,
        options: &RendererOptions,
        window: &Window,
        clear_colour: Colour,
    ) -> Result<Self, String> {
        Ok(Self(
            Box::new(match backend {
                #[cfg(not(target_arch = "wasm32"))]
                () => opengl::RendererImpl::new(options, connection, window, clear_colour)?,
                #[cfg(target_arch = "wasm32")]
                () => webgl::RendererImpl::new(options, connection, window, clear_colour)?,
            }),
            false,
        ))
    }

    /// Makes every primitive begun from now on keep its texture coordinates, whether or not it was
    /// begun with a texture.
    ///
    /// A primitive with no texture normally has its coordinates replaced with NaN, which is how the
    /// built-in fragment shader is told there is nothing to sample. That is right for a draw and
    /// wrong for a *model cache*, which outlives the draw that filled it: `d3d_model_draw` takes its
    /// texture per draw, and GM8.2 fills every cache with no texture at all, since `d3d_model_bake`
    /// is defined as `d3d_model_draw(m,0,0,0,-1)`. Whether a draw has a texture is decided per draw
    /// instead -- see `draw_buffers`.
    ///
    /// Set here rather than at each `reset_primitive_3d`, because a model's shapes (`d3d_model_block`
    /// and friends) begin primitives of their own several times each, deep inside `model.rs`.
    pub fn keep_tex_coords(&mut self, keep: bool) {
        self.1 = keep;
    }

    pub fn max_texture_size(&self) -> u32 {
        self.0.max_texture_size()
    }

    pub fn push_atlases(&mut self, atl: atlas::AtlasBuilder) -> Result<(), String> {
        self.0.push_atlases(atl)
    }

    pub fn upload_sprite(
        &mut self,
        data: Box<[u8]>,
        width: i32,
        height: i32,
        origin_x: i32,
        origin_y: i32,
    ) -> Result<AtlasRef, String> {
        self.0.upload_sprite(data, width, height, origin_x, origin_y)
    }

    pub fn duplicate_sprite(&mut self, atlas_ref: AtlasRef) -> Result<AtlasRef, String> {
        self.0.duplicate_sprite(atlas_ref)
    }

    pub fn set_sprite_origin(&mut self, atlas_ref: AtlasRef, origin_x: i32, origin_y: i32) {
        self.0.set_sprite_origin(atlas_ref, origin_x, origin_y)
    }

    pub fn delete_sprite(&mut self, atlas_ref: AtlasRef) {
        self.0.delete_sprite(atlas_ref)
    }

    pub fn check_texture_id(&self, id: i32) -> bool {
        self.0.get_rect(AtlasRef(id)).is_some()
    }

    pub fn set_vsync(&self, vsync: bool) {
        self.0.set_vsync(vsync)
    }

    pub fn get_vsync(&self) -> bool {
        self.0.get_vsync()
    }

    pub fn wait_vsync(&self) {
        self.0.wait_vsync()
    }

    pub fn draw_sprite(
        &mut self,
        texture: AtlasRef,
        x: f64,
        y: f64,
        xscale: f64,
        yscale: f64,
        angle: f64,
        colour: i32,
        alpha: f64,
    ) {
        self.0.draw_sprite(texture, x, y, xscale, yscale, angle, colour, alpha)
    }

    pub fn draw_sprite_colour(
        &mut self,
        tex: AtlasRef,
        x: f64,
        y: f64,
        xs: f64,
        ys: f64,
        ang: f64,
        col1: i32,
        col2: i32,
        col3: i32,
        col4: i32,
        alpha: f64,
    ) {
        self.0.draw_sprite_colour(tex, x, y, xs, ys, ang, col1, col2, col3, col4, alpha)
    }

    pub fn draw_sprite_general(
        &mut self,
        texture: AtlasRef,
        part_x: f64,
        part_y: f64,
        part_w: f64,
        part_h: f64,
        x: f64,
        y: f64,
        xscale: f64,
        yscale: f64,
        angle: f64,
        col1: i32,
        col2: i32,
        col3: i32,
        col4: i32,
        alpha: f64,
        use_origin: bool,
    ) {
        self.0.draw_sprite_general(
            texture, part_x, part_y, part_w, part_h, x, y, xscale, yscale, angle, col1, col2, col3, col4, alpha,
            use_origin,
        )
    }

    pub fn draw_sprite_pos(
        &mut self,
        texture: AtlasRef,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        x3: f64,
        y3: f64,
        x4: f64,
        y4: f64,
        alpha: f64,
    ) {
        self.0.draw_sprite_pos(texture, x1, y1, x2, y2, x3, y3, x4, y4, alpha)
    }

    pub fn set_view_matrix(&mut self, view: [f32; 16]) {
        self.0.set_view_matrix(view)
    }

    pub fn set_viewproj_matrix(&mut self, view: [f32; 16], proj: [f32; 16]) {
        self.0.set_viewproj_matrix(view, proj)
    }

    pub fn get_view_matrix(&self) -> [f32; 16] {
        self.0.get_view_matrix()
    }

    pub fn get_proj_matrix(&self) -> [f32; 16] {
        self.0.get_proj_matrix()
    }

    pub fn get_model_matrix(&self) -> [f32; 16] {
        self.0.get_model_matrix()
    }

    pub fn set_model_matrix(&mut self, model: [f32; 16]) {
        self.0.set_model_matrix(model)
    }

    pub fn mult_model_matrix(&mut self, model: [f32; 16]) {
        self.0.mult_model_matrix(model)
    }

    pub fn set_projection_ortho(&mut self, x: f64, y: f64, w: f64, h: f64, angle: f64) {
        self.0.set_projection_ortho(x, y, w, h, angle)
    }

    pub fn set_projection_perspective(&mut self, x: f64, y: f64, w: f64, h: f64, angle: f64) {
        self.0.set_projection_perspective(x, y, w, h, angle)
    }

    pub fn set_view(
        &mut self,
        src_x: i32,
        src_y: i32,
        src_w: i32,
        src_h: i32,
        src_angle: f64,
        port_x: i32,
        port_y: i32,
        port_w: i32,
        port_h: i32,
    ) {
        self.0.set_view(src_x, src_y, src_w, src_h, src_angle, port_x, port_y, port_w, port_h)
    }

    pub fn get_viewport(&self) -> (i32, i32, i32, i32) {
        self.0.get_viewport()
    }

    pub fn set_viewport(&mut self, x: i32, y: i32, w: i32, h: i32) {
        self.0.set_viewport(x, y, w, h)
    }

    pub fn draw_sprite_partial(
        &mut self,
        texture: AtlasRef,
        part_x: f64,
        part_y: f64,
        part_w: f64,
        part_h: f64,
        x: f64,
        y: f64,
        xscale: f64,
        yscale: f64,
        angle: f64,
        colour: i32,
        alpha: f64,
    ) {
        self.0.draw_sprite_partial(texture, part_x, part_y, part_w, part_h, x, y, xscale, yscale, angle, colour, alpha)
    }

    pub fn draw_sprite_tiled(
        &mut self,
        texture: AtlasRef,
        x: f64,
        y: f64,
        xscale: f64,
        yscale: f64,
        colour: i32,
        alpha: f64,
        tile_end_x: Option<f64>,
        tile_end_y: Option<f64>,
    ) {
        self.0.draw_sprite_tiled(texture, x, y, xscale, yscale, colour, alpha, tile_end_x, tile_end_y)
    }

    pub fn draw_rectangle(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, colour: i32, alpha: f64) {
        self.0.draw_rectangle(x1, y1, x2, y2, colour, alpha)
    }

    pub fn draw_rectangle_outline(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, colour: i32, alpha: f64) {
        self.0.draw_rectangle_outline(x1, y1, x2, y2, colour, alpha)
    }

    pub fn draw_rectangle_gradient(
        &mut self,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        c1: i32,
        c2: i32,
        c3: i32,
        c4: i32,
        alpha: f64,
        outline: bool,
    ) {
        self.0.draw_rectangle_gradient(x1, y1, x2, y2, c1, c2, c3, c4, alpha, outline)
    }

    pub fn draw_point(&mut self, x: f64, y: f64, colour: i32, alpha: f64) {
        self.0.draw_point(x, y, colour, alpha)
    }

    pub fn draw_line(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, width: Option<f64>, c1: i32, c2: i32, alpha: f64) {
        self.0.draw_line(x1, y1, x2, y2, width, c1, c2, alpha)
    }

    pub fn draw_triangle(
        &mut self,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        x3: f64,
        y3: f64,
        c1: i32,
        c2: i32,
        c3: i32,
        alpha: f64,
        outline: bool,
    ) {
        self.0.draw_triangle(x1, y1, x2, y2, x3, y3, c1, c2, c3, alpha, outline)
    }

    pub fn draw_ellipse(
        &mut self,
        x: f64,
        y: f64,
        rad_x: f64,
        rad_y: f64,
        c1: i32,
        c2: i32,
        alpha: f64,
        outline: bool,
    ) {
        self.0.draw_ellipse(x, y, rad_x, rad_y, c1, c2, alpha, outline)
    }

    pub fn draw_roundrect(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, c1: i32, c2: i32, alpha: f64, outline: bool) {
        self.0.draw_roundrect(x1, y1, x2, y2, c1, c2, alpha, outline)
    }

    pub fn set_circle_precision(&mut self, prec: i32) {
        self.0.set_circle_precision(prec)
    }

    pub fn get_circle_precision(&self) -> i32 {
        self.0.get_circle_precision()
    }

    pub fn reset_primitive_2d(&mut self, ptype: PrimitiveType, atlas_ref: Option<AtlasRef>) {
        self.0.reset_primitive_2d(ptype, atlas_ref)
    }

    pub fn vertex_2d(&mut self, x: f64, y: f64, xtex: f64, ytex: f64, col: i32, alpha: f64) {
        self.0.vertex_2d(x, y, xtex, ytex, col, alpha)
    }

    pub fn draw_primitive_2d(&mut self) {
        self.0.draw_primitive_2d()
    }

    pub fn get_primitive_2d(&self) -> PrimitiveBuilder {
        self.0.get_primitive_2d()
    }

    pub fn set_primitive_2d(&mut self, prim: PrimitiveBuilder) {
        self.0.set_primitive_2d(prim)
    }

    pub fn reset_primitive_3d(&mut self, ptype: PrimitiveType, atlas_ref: Option<AtlasRef>) {
        self.0.reset_primitive_3d(ptype, atlas_ref);
        if self.1 {
            let mut prim = self.0.get_primitive_3d();
            prim.keep_tex_coords();
            self.0.set_primitive_3d(prim);
        }
    }

    pub fn vertex_3d(
        &mut self,
        x: f64,
        y: f64,
        z: f64,
        nx: f64,
        ny: f64,
        nz: f64,
        xtex: f64,
        ytex: f64,
        col: i32,
        alpha: f64,
    ) {
        self.0.vertex_3d(x, y, z, nx, ny, nz, xtex, ytex, col, alpha)
    }

    pub fn draw_primitive_3d(&mut self) {
        self.0.draw_primitive_3d()
    }

    pub fn get_primitive_3d(&self) -> PrimitiveBuilder {
        self.0.get_primitive_3d()
    }

    pub fn set_primitive_3d(&mut self, prim: PrimitiveBuilder) {
        self.0.set_primitive_3d(prim)
    }

    pub fn extend_buffers(&self, buf: &mut VertexBuffer) {
        self.0.extend_buffers(buf)
    }

    pub fn draw_buffers(&mut self, atlas_ref: Option<AtlasRef>, buf: &VertexBuffer) {
        self.0.draw_buffers(atlas_ref, buf)
    }

    pub fn dump_sprite(&self, atlas_ref: AtlasRef) -> Box<[u8]> {
        self.0.dump_sprite(atlas_ref)
    }

    pub fn dump_sprite_part(&self, texture: AtlasRef, part_x: i32, part_y: i32, part_w: i32, part_h: i32) -> Box<[u8]> {
        self.0.dump_sprite_part(texture, part_x, part_y, part_w, part_h)
    }

    pub fn resize_framebuffer(&mut self, width: u32, height: u32, store: bool) {
        self.0.resize_framebuffer(width, height, store)
    }

    pub fn get_pixels(&self, x: i32, y: i32, w: i32, h: i32) -> Box<[u8]> {
        self.0.get_pixels(x, y, w, h)
    }

    pub fn stored_pixels(&self) -> Box<[u8]> {
        self.0.stored_pixels()
    }

    pub fn stored_zbuffer(&self) -> Box<[f32]> {
        self.0.stored_zbuffer()
    }

    pub fn set_stored(&mut self, rgba: Box<[u8]>, zbuf: Box<[f32]>, fb_w: u32, fb_h: u32) {
        self.0.set_stored(rgba, zbuf, fb_w, fb_h)
    }

    pub fn dump_dynamic_textures(&self) -> Vec<Option<SavedTexture>> {
        self.0.dump_dynamic_textures()
    }

    pub fn upload_dynamic_textures(&mut self, textures: &[Option<SavedTexture>]) {
        self.0.upload_dynamic_textures(textures)
    }

    pub fn create_sprite_colour(&mut self, width: i32, height: i32, col: Colour) -> Result<AtlasRef, String> {
        self.0.create_sprite_colour(width, height, col)
    }

    pub fn create_surface(&mut self, w: i32, h: i32, has_zbuffer: bool) -> Result<AtlasRef, String> {
        self.0.create_surface(w, h, has_zbuffer)
    }

    pub fn set_target(&mut self, atlas_ref: AtlasRef) {
        self.0.set_target(atlas_ref)
    }

    pub fn reset_target(&mut self) {
        self.0.reset_target()
    }

    pub fn copy_surface(
        &mut self,
        dest: AtlasRef,
        dest_x: i32,
        dest_y: i32,
        src: AtlasRef,
        src_x: i32,
        src_y: i32,
        width: i32,
        height: i32,
    ) {
        self.0.copy_surface(dest, dest_x, dest_y, src, src_x, src_y, width, height)
    }

    pub fn set_zbuf_trashed(&mut self, trashed: bool) {
        self.0.set_zbuf_trashed(trashed)
    }

    pub fn get_zbuf_trashed(&self) -> bool {
        self.0.get_zbuf_trashed()
    }

    pub fn get_texture_id(&mut self, atl_ref: AtlasRef) -> i32 {
        self.0.get_texture_id(atl_ref)
    }

    pub fn get_texture_from_id(&self, id: i32) -> Option<AtlasRef> {
        self.0.get_texture_from_id(id)
    }

    pub fn get_texture_rects(&self) -> Vec<Option<AtlasRect>> {
        self.0.get_texture_rects()
    }

    pub fn set_texture_rects(&mut self, rects: &[Option<AtlasRect>]) {
        self.0.set_texture_rects(rects)
    }

    pub fn get_alpha_blending(&self) -> bool {
        self.0.get_alpha_blending()
    }

    pub fn set_alpha_blending(&mut self, alphablend: bool) {
        self.0.set_alpha_blending(alphablend)
    }

    pub fn get_colour_blending(&self) -> bool {
        self.0.get_colour_blending()
    }

    pub fn set_colour_blending(&mut self, modulate: bool) {
        self.0.set_colour_blending(modulate)
    }

    pub fn get_blend_mode(&self) -> (BlendType, BlendType) {
        self.0.get_blend_mode()
    }

    pub fn set_blend_mode(&mut self, src: BlendType, dst: BlendType) {
        self.0.set_blend_mode(src, dst)
    }

    pub fn set_blend_mode_alpha(&mut self, modes: Option<(BlendType, BlendType)>) {
        self.0.set_blend_mode_alpha(modes)
    }

    pub fn set_colour_mask(&mut self, r: bool, g: bool, b: bool, a: bool) {
        self.0.set_colour_mask(r, g, b, a)
    }

    pub fn set_depth_func(&mut self, func: DepthFunc) {
        self.0.set_depth_func(func)
    }

    pub fn set_depth_bias(&mut self, bias: f64) {
        self.0.set_depth_bias(bias)
    }

    pub fn create_shader(&mut self, stage: ShaderStage, glsl: &str) -> Result<ShaderRef, String> {
        self.0.create_shader(stage, glsl)
    }

    pub fn delete_shader(&mut self, shader: ShaderRef) {
        self.0.delete_shader(shader)
    }

    pub fn set_shader_stage(&mut self, stage: ShaderStage, shader: Option<ShaderRef>) {
        self.0.set_shader_stage(stage, shader)
    }

    pub fn get_shader_stage(&self, stage: ShaderStage) -> Option<ShaderRef> {
        self.0.get_shader_stage(stage)
    }

    pub fn set_shader_constant(&mut self, stage: ShaderStage, register: u32, value: [f32; 4]) {
        self.0.set_shader_constant(stage, register, value)
    }

    pub fn set_shader_sampler(&mut self, kind: ShaderStage, stage: u32, texture: Option<AtlasRef>) {
        self.0.set_shader_sampler(kind, stage, texture)
    }

    pub fn set_shader_sampler_state(
        &mut self,
        kind: ShaderStage,
        stage: u32,
        interpolate: Option<bool>,
        repeat: Option<(bool, bool)>,
    ) {
        self.0.set_shader_sampler_state(kind, stage, interpolate, repeat)
    }

    pub fn get_viewproj_matrix(&self) -> [f32; 16] {
        self.0.get_viewproj_matrix()
    }

    pub fn draw_vertex_streams(
        &mut self,
        streams: &[VertexStream],
        indices: Option<IndexData>,
        primitive: PrimitiveType,
        vertex_count: usize,
        instance_count: usize,
        texture: Option<AtlasRef>,
    ) -> bool {
        self.0.draw_vertex_streams(streams, indices, primitive, vertex_count, instance_count, texture)
    }

    pub fn set_depth_range(&mut self, near: f64, far: f64) {
        self.0.set_depth_range(near, far)
    }

    pub fn get_pixel_interpolation(&self) -> bool {
        self.0.get_pixel_interpolation()
    }

    pub fn set_pixel_interpolation(&mut self, lerping: bool) {
        self.0.set_pixel_interpolation(lerping)
    }

    pub fn get_texture_repeat(&self) -> bool {
        self.0.get_texture_repeat()
    }

    pub fn set_texture_repeat(&mut self, repeat: bool) {
        self.0.set_texture_repeat(repeat)
    }

    pub fn flush_queue(&mut self) {
        self.0.flush_queue()
    }

    pub fn clear_view(&mut self, colour: Colour, alpha: f64) {
        self.0.clear_view(colour, alpha)
    }

    pub fn clear_view_no_zbuf(&mut self, colour: Colour, alpha: f64) {
        self.0.clear_view_no_zbuf(colour, alpha)
    }

    pub fn clear_zbuf(&mut self) {
        self.0.clear_zbuf()
    }

    pub fn get_3d(&self) -> bool {
        self.0.get_3d()
    }

    pub fn set_3d(&mut self, use_3d: bool) {
        self.0.set_3d(use_3d)
    }

    pub fn get_depth(&self) -> f32 {
        self.0.get_depth()
    }

    pub fn set_depth(&mut self, depth: f32) {
        self.0.set_depth(depth)
    }

    pub fn get_depth_test(&self) -> bool {
        self.0.get_depth_test()
    }

    pub fn set_depth_test(&mut self, depth_test: bool) {
        self.0.set_depth_test(depth_test)
    }

    pub fn get_write_depth(&self) -> bool {
        self.0.get_write_depth()
    }

    pub fn set_write_depth(&mut self, write_depth: bool) {
        self.0.set_write_depth(write_depth)
    }

    pub fn get_culling(&self) -> bool {
        self.0.get_culling()
    }

    pub fn set_culling(&mut self, culling: bool) {
        self.0.set_culling(culling)
    }

    pub fn set_cull_clockwise(&mut self, clockwise: bool) {
        self.0.set_cull_clockwise(clockwise)
    }

    pub fn get_perspective(&self) -> bool {
        self.0.get_perspective()
    }

    pub fn set_perspective(&mut self, perspective: bool) {
        self.0.set_perspective(perspective)
    }

    pub fn get_fog(&self) -> Option<Fog> {
        self.0.get_fog()
    }

    pub fn set_fog(&mut self, fog: Option<Fog>) {
        self.0.set_fog(fog)
    }

    pub fn get_gouraud(&self) -> bool {
        self.0.get_gouraud()
    }

    pub fn set_gouraud(&mut self, gouraud: bool) {
        self.0.set_gouraud(gouraud)
    }

    pub fn get_lighting_enabled(&self) -> bool {
        self.0.get_lighting_enabled()
    }

    pub fn set_lighting_enabled(&mut self, enabled: bool) {
        self.0.set_lighting_enabled(enabled)
    }

    pub fn get_ambient_colour(&self) -> i32 {
        self.0.get_ambient_colour()
    }

    pub fn set_ambient_colour(&mut self, colour: i32) {
        self.0.set_ambient_colour(colour)
    }

    pub fn get_lights(&self) -> [(bool, Light); 8] {
        self.0.get_lights()
    }

    pub fn set_lights(&mut self, lights: [(bool, Light); 8]) {
        self.0.set_lights(lights)
    }

    pub fn set_light_enabled(&mut self, id: usize, enabled: bool) {
        self.0.set_light_enabled(id, enabled)
    }

    pub fn set_light(&mut self, id: usize, light: Light) {
        self.0.set_light(id, light)
    }

    pub fn get_texcoord_source(&self) -> TexcoordSource {
        self.0.get_texcoord_source()
    }

    pub fn set_texcoord_source(&mut self, source: TexcoordSource) {
        self.0.set_texcoord_source(source)
    }

    pub fn present(&mut self, window_width: u32, window_height: u32, scaling: Scaling) {
        self.0.present(window_width, window_height, scaling)
    }

    pub fn draw_stored(&mut self, target_x: i32, target_y: i32, width: u32, height: u32) {
        self.0.draw_stored(target_x, target_y, width, height)
    }

    pub fn stored_size(&self) -> (u32, u32) {
        self.0.stored_size()
    }

    pub fn finish(&mut self, window_width: u32, window_height: u32, clear_colour: Colour) {
        self.0.finish(window_width, window_height, clear_colour)
    }

    pub fn state(&self) -> RendererState {
        RendererState {
            model_matrix: self.get_model_matrix(),
            alpha_blending: self.get_alpha_blending(),
            colour_blending: self.get_colour_blending(),
            blend_mode: self.get_blend_mode(),
            pixel_interpolation: self.get_pixel_interpolation(),
            texture_repeat: self.get_texture_repeat(),
            texture_rects: self.get_texture_rects(),
            vsync: self.get_vsync(),
            ambient_colour: self.get_ambient_colour(),
            using_3d: self.get_3d(),
            depth: self.get_depth(),
            depth_test: self.get_depth_test(),
            write_depth: self.get_write_depth(),
            culling: self.get_culling(),
            perspective: self.get_perspective(),
            fog: self.get_fog(),
            gouraud: self.get_gouraud(),
            lighting_enabled: self.get_lighting_enabled(),
            lights: self.get_lights(),
            circle_precision: self.get_circle_precision(),
            primitive_2d: self.get_primitive_2d(),
            primitive_3d: self.get_primitive_3d(),
            zbuf_trashed: self.get_zbuf_trashed(),
        }
    }

    pub fn set_state(&mut self, state: &RendererState) {
        self.set_model_matrix(state.model_matrix);
        self.set_alpha_blending(state.alpha_blending);
        self.set_colour_blending(state.colour_blending);
        self.set_blend_mode(state.blend_mode.0, state.blend_mode.1);
        self.set_pixel_interpolation(state.pixel_interpolation);
        self.set_texture_repeat(state.texture_repeat);
        self.set_texture_rects(&state.texture_rects);
        self.set_vsync(state.vsync);
        self.set_ambient_colour(state.ambient_colour);
        self.set_3d(state.using_3d);
        self.set_depth(state.depth);
        self.set_depth_test(state.depth_test);
        self.set_write_depth(state.write_depth);
        self.set_culling(state.culling);
        self.set_perspective(state.perspective);
        self.set_fog(state.fog.clone());
        self.set_gouraud(state.gouraud);
        self.set_lighting_enabled(state.lighting_enabled);
        self.set_lights(state.lights);
        self.set_circle_precision(state.circle_precision);
        self.set_primitive_2d(state.primitive_2d.clone());
        self.set_primitive_3d(state.primitive_3d.clone());
        self.set_zbuf_trashed(state.zbuf_trashed);
    }
}

/// Easy wrapper for all the parts of the renderer which need saving in savestates.
/// Everything in this struct can be queried individually. Don't use this for querying individual things.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RendererState {
    pub model_matrix: [f32; 16],
    pub alpha_blending: bool,
    pub colour_blending: bool,
    pub blend_mode: (BlendType, BlendType),
    pub pixel_interpolation: bool,
    pub texture_repeat: bool,
    pub texture_rects: Vec<Option<AtlasRect>>,
    pub vsync: bool,
    pub ambient_colour: i32,
    pub using_3d: bool,
    pub depth: f32,
    pub depth_test: bool,
    pub write_depth: bool,
    pub culling: bool,
    pub perspective: bool,
    pub fog: Option<Fog>,
    pub gouraud: bool,
    pub lighting_enabled: bool,
    pub lights: [(bool, Light); 8],
    pub circle_precision: i32,
    pub primitive_2d: PrimitiveBuilder,
    pub primitive_3d: PrimitiveBuilder,
    pub zbuf_trashed: bool,
}

/// Multiply two mat4's together
pub fn mat4mult(m1: [f32; 16], m2: [f32; 16]) -> [f32; 16] {
    [
        (m1[0] * m2[0]) + (m1[1] * m2[4]) + (m1[2] * m2[8]) + (m1[3] * m2[12]),
        (m1[0] * m2[1]) + (m1[1] * m2[5]) + (m1[2] * m2[9]) + (m1[3] * m2[13]),
        (m1[0] * m2[2]) + (m1[1] * m2[6]) + (m1[2] * m2[10]) + (m1[3] * m2[14]),
        (m1[0] * m2[3]) + (m1[1] * m2[7]) + (m1[2] * m2[11]) + (m1[3] * m2[15]),
        (m1[4] * m2[0]) + (m1[5] * m2[4]) + (m1[6] * m2[8]) + (m1[7] * m2[12]),
        (m1[4] * m2[1]) + (m1[5] * m2[5]) + (m1[6] * m2[9]) + (m1[7] * m2[13]),
        (m1[4] * m2[2]) + (m1[5] * m2[6]) + (m1[6] * m2[10]) + (m1[7] * m2[14]),
        (m1[4] * m2[3]) + (m1[5] * m2[7]) + (m1[6] * m2[11]) + (m1[7] * m2[15]),
        (m1[8] * m2[0]) + (m1[9] * m2[4]) + (m1[10] * m2[8]) + (m1[11] * m2[12]),
        (m1[8] * m2[1]) + (m1[9] * m2[5]) + (m1[10] * m2[9]) + (m1[11] * m2[13]),
        (m1[8] * m2[2]) + (m1[9] * m2[6]) + (m1[10] * m2[10]) + (m1[11] * m2[14]),
        (m1[8] * m2[3]) + (m1[9] * m2[7]) + (m1[10] * m2[11]) + (m1[11] * m2[15]),
        (m1[12] * m2[0]) + (m1[13] * m2[4]) + (m1[14] * m2[8]) + (m1[15] * m2[12]),
        (m1[12] * m2[1]) + (m1[13] * m2[5]) + (m1[14] * m2[9]) + (m1[15] * m2[13]),
        (m1[12] * m2[2]) + (m1[13] * m2[6]) + (m1[14] * m2[10]) + (m1[15] * m2[14]),
        (m1[12] * m2[3]) + (m1[13] * m2[7]) + (m1[14] * m2[11]) + (m1[15] * m2[15]),
    ]
}
