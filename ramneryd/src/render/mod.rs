use std::mem::MaybeUninit;

use thiserror::Error;

use crate::ecs::prelude::*;

use trekanten::mem::{BufferMutability, OwningUniformBufferDescriptor, UniformBuffer};
use trekanten::pipeline::{
    GraphicsPipeline, GraphicsPipelineDescriptor, PipelineError, ShaderDescriptor,
};
use trekanten::resource::Handle;
use trekanten::resource::ResourceManager;
use trekanten::util;
use trekanten::vertex::VertexFormat;
use trekanten::BufferHandle;
use trekanten::RenderPassEncoder;
use trekanten::Renderer;
use trekanten::{
    descriptor::DescriptorSet, texture::SamplerDescriptor, texture::TextureDescriptor,
    texture::TextureUsage,
};

mod bounding_box;
pub mod debug_window;
pub mod geometry;
pub mod light;
pub mod material;
pub mod mesh;
pub mod pipeline;
pub mod ui;
pub mod uniform;

pub use light::Light;

use mesh::GpuMesh;
use mesh::PendingMesh;

use crate::camera::*;
use crate::ecs;
use crate::math::{Mat4, ModelMatrix, Transform, Vec3};
use material::{GpuMaterial, PendingMaterial};
use ramneryd_derive::Inspect;

pub fn camera_pos(world: &World) -> Vec3 {
    let camera_entity = ecs::get_singleton_entity::<Camera>(world);
    let transforms = world.read_storage::<Transform>();
    transforms
        .get(camera_entity)
        .expect("Could not get position component for camera")
        .position
}

struct SpotlightShadow {
    render_target: Handle<trekanten::RenderTarget>,
    view_data_buffer: BufferHandle<UniformBuffer>,
    view_data_desc_set: Handle<DescriptorSet>,
    texture: Handle<trekanten::Texture>,
}

const NUM_SPOTLIGHT_SHADOW_MAPS: usize = 16;

struct ShadowData {
    render_pass: Handle<trekanten::RenderPass>,
    dummy_pipeline: Handle<GraphicsPipeline>,
    spotlights: [SpotlightShadow; NUM_SPOTLIGHT_SHADOW_MAPS],
}

struct UnlitFrameUniformResources {
    dummy_pipeline: Handle<GraphicsPipeline>,
    shader_resource_group: Handle<DescriptorSet>,
}

struct PhysicallyBasedUniformResources {
    dummy_pipeline: Handle<GraphicsPipeline>,
    shader_resource_group: Handle<DescriptorSet>,
    light_buffer: BufferHandle<UniformBuffer>,
    shadow_matrices_buffer: BufferHandle<UniformBuffer>,
}

pub struct FrameData {
    main_render_pass: Handle<trekanten::RenderPass>,
    main_camera_view_data: BufferHandle<UniformBuffer>,
    unlit_resources: UnlitFrameUniformResources,
    pbr_resources: PhysicallyBasedUniformResources,
    shadow: ShadowData,
}

fn get_view_data(world: &World) -> (Mat4, Vec3) {
    let camera_entity = ecs::get_singleton_entity::<Camera>(world);
    let transforms = world.read_storage::<Transform>();
    let rots = world.read_storage::<CameraRotationState>();

    let cam_pos = transforms
        .get(camera_entity)
        .expect("Could not get position component for camera")
        .position;

    let cam_rotation_state = rots
        .get(camera_entity)
        .expect("Could not get rotation state for camera");

    // TODO: Camera system should write to ViewMatrixResource at the end of system
    // and we should read it here.
    let view = FreeFlyCameraController::get_view_matrix_from(cam_pos, cam_rotation_state);
    log::trace!("View matrix: {:#?}", view);

    (view, cam_pos)
}

fn get_proj_matrix(aspect_ratio: f32) -> Mat4 {
    crate::math::perspective_vk(std::f32::consts::FRAC_PI_4, aspect_ratio, 0.05, 1000000.0)
}

#[derive(Component, Default)]
#[component(storage = "NullStorage")]
pub struct ReloadMaterial;

#[derive(Component)]
#[component(inspect)]
pub enum RenderableMaterial {
    PBR {
        gfx_pipeline: Handle<GraphicsPipeline>,
        shadow_pipeline: Handle<GraphicsPipeline>,
        material_descriptor_set: Handle<DescriptorSet>,
    },
    Unlit {
        gfx_pipeline: Handle<GraphicsPipeline>,
        material_descriptor_set: Handle<DescriptorSet>,
    },
}

impl RenderableMaterial {
    fn set_pipeline(&mut self, h: Handle<GraphicsPipeline>) {
        match self {
            RenderableMaterial::PBR { gfx_pipeline, .. } => *gfx_pipeline = h,
            RenderableMaterial::Unlit { gfx_pipeline, .. } => *gfx_pipeline = h,
        }
    }
}

// TODO: Bindings here need to match with shader
fn create_material_descriptor_set(
    renderer: &mut Renderer,
    material: &GpuMaterial,
) -> Handle<DescriptorSet> {
    match &material {
        material::GpuMaterial::PBR {
            material_uniforms,
            normal_map,
            base_color_texture,
            metallic_roughness_texture,
            ..
        } => {
            let mut desc_set_builder = DescriptorSet::builder(renderer);

            desc_set_builder = desc_set_builder.add_buffer(
                &material_uniforms,
                0,
                trekanten::pipeline::ShaderStage::FRAGMENT,
            );

            if let Some(bct) = &base_color_texture {
                desc_set_builder = desc_set_builder.add_texture(
                    &bct.handle,
                    1,
                    trekanten::pipeline::ShaderStage::FRAGMENT,
                    false,
                );
            }

            if let Some(mrt) = &metallic_roughness_texture {
                desc_set_builder = desc_set_builder.add_texture(
                    &mrt.handle,
                    2,
                    trekanten::pipeline::ShaderStage::FRAGMENT,
                    false,
                );
            }

            if let Some(nm) = &normal_map {
                desc_set_builder = desc_set_builder.add_texture(
                    &nm.handle,
                    3,
                    trekanten::pipeline::ShaderStage::FRAGMENT,
                    false,
                );
            }

            desc_set_builder.build()
        }
        material::GpuMaterial::Unlit { color_uniform } => DescriptorSet::builder(renderer)
            .add_buffer(
                &color_uniform,
                0,
                trekanten::pipeline::ShaderStage::FRAGMENT,
            )
            .build(),
    }
}

#[derive(Debug, Error)]
pub enum MaterialError {
    #[error("Pipeline error: {0}")]
    Pipeline(#[from] PipelineError),
    #[error("GLSL compiler error: {0}")]
    GlslCompiler(#[from] pipeline::CompilerError),
}

fn unlit_pipeline_desc(
    shader_compiler: &pipeline::ShaderCompiler,
    vertex_format: VertexFormat,
    polygon_mode: trekanten::pipeline::PolygonMode,
) -> Result<GraphicsPipelineDescriptor, MaterialError> {
    let vertex = shader_compiler.compile(
        &pipeline::Defines::empty(),
        "pos_only_vert.glsl",
        pipeline::ShaderType::Vertex,
    )?;
    let fragment = shader_compiler.compile(
        &pipeline::Defines::empty(),
        "uniform_color_frag.glsl",
        pipeline::ShaderType::Fragment,
    )?;

    Ok(GraphicsPipelineDescriptor::builder()
        .vert(ShaderDescriptor::FromRawSpirv(vertex.data()))
        .frag(ShaderDescriptor::FromRawSpirv(fragment.data()))
        .vertex_format(vertex_format)
        .culling(trekanten::pipeline::TriangleCulling::None)
        .polygon_mode(polygon_mode)
        .build()?)
}

fn get_pipeline_for(
    renderer: &mut Renderer,
    world: &World,
    mesh: &GpuMesh,
    mat: &material::GpuMaterial,
) -> Result<Handle<GraphicsPipeline>, MaterialError> {
    // TODO: Infer from spirv?
    let vertex_format = renderer
        .get_resource(&mesh.vertex_buffer)
        .expect("Invalid handle")
        .format()
        .clone();

    let frame_data = world.read_resource::<FrameData>();
    let shader_compiler = world.read_resource::<pipeline::ShaderCompiler>();
    let pipe = match mat {
        material::GpuMaterial::PBR {
            normal_map,
            base_color_texture,
            metallic_roughness_texture,
            has_vertex_colors,
            ..
        } => {
            // TODO: Normal map does not infer tangents at all times
            let has_nm = normal_map.is_some();
            let has_bc = base_color_texture.is_some();
            let has_mr = metallic_roughness_texture.is_some();
            let def = pipeline::pbr_gltf::ShaderDefinition {
                has_tex_coords: has_nm || has_bc || has_mr,
                has_vertex_colors: *has_vertex_colors,
                has_tangents: has_nm,
                has_base_color_texture: has_bc,
                has_metallic_roughness_texture: has_mr,
                has_normal_map: has_nm,
            };

            let (vert, frag) = pipeline::pbr_gltf::compile(&*shader_compiler, &def)?;
            let desc = GraphicsPipelineDescriptor::builder()
                .vert(ShaderDescriptor::FromRawSpirv(vert.data()))
                .frag(ShaderDescriptor::FromRawSpirv(frag.data()))
                .vertex_format(vertex_format)
                .polygon_mode(mesh.polygon_mode)
                .build()?;

            renderer.create_gfx_pipeline(desc, &frame_data.main_render_pass)?
        }
        material::GpuMaterial::Unlit { .. } => {
            let desc = unlit_pipeline_desc(&shader_compiler, vertex_format, mesh.polygon_mode)?;
            renderer.create_gfx_pipeline(desc, &frame_data.main_render_pass)?
        }
    };

    Ok(pipe)
}

fn shadow_pipeline_desc(
    shader_compiler: &pipeline::ShaderCompiler,
    format: VertexFormat,
) -> Result<GraphicsPipelineDescriptor, MaterialError> {
    let no_defines = pipeline::Defines::empty();
    let vert = shader_compiler.compile(
        &no_defines,
        "pos_only_vert.glsl",
        pipeline::ShaderType::Vertex,
    )?;

    Ok(GraphicsPipelineDescriptor::builder()
        .vertex_format(format)
        .vert(ShaderDescriptor::FromRawSpirv(vert.data()))
        .culling(trekanten::pipeline::TriangleCulling::Front)
        .build()?)
}

fn get_shadow_pipeline_for(
    renderer: &mut Renderer,
    world: &World,
    mesh: &GpuMesh,
) -> Result<Handle<GraphicsPipeline>, MaterialError> {
    let shader_compiler = world.read_resource::<pipeline::ShaderCompiler>();
    let frame_data = world.read_resource::<FrameData>();

    let vertex_format_size = renderer
        .get_resource(&mesh.vertex_buffer)
        .expect("Invalid handle")
        .format()
        .size();

    let shadow_vertex_format = trekanten::vertex::VertexFormat::builder()
        .add_attribute(trekanten::util::Format::FLOAT3) // pos
        .skip(vertex_format_size - trekanten::util::Format::FLOAT3.size())
        .build();
    let descriptor = shadow_pipeline_desc(&shader_compiler, shadow_vertex_format)?;
    Ok(renderer.create_gfx_pipeline(descriptor, &frame_data.shadow.render_pass)?)
}

fn create_renderable(
    renderer: &mut Renderer,
    world: &World,
    mesh: &GpuMesh,
    material: &GpuMaterial,
) -> RenderableMaterial {
    log::trace!("Creating renderable: {:?}", material);
    let material_descriptor_set = create_material_descriptor_set(renderer, material);
    let gfx_pipeline =
        get_pipeline_for(renderer, world, mesh, &material).expect("Failed to get pipeline");
    match material {
        material::GpuMaterial::PBR { .. } => RenderableMaterial::PBR {
            gfx_pipeline,
            shadow_pipeline: get_shadow_pipeline_for(renderer, world, mesh)
                .expect("Failed to create shadow pipeline"),
            material_descriptor_set,
        },
        material::GpuMaterial::Unlit { .. } => RenderableMaterial::Unlit {
            gfx_pipeline,
            material_descriptor_set,
        },
    }
}

#[profiling::function]
fn create_renderables(renderer: &mut Renderer, world: &mut World) {
    use specs::storage::StorageEntry;

    let meshes = world.read_storage::<GpuMesh>();
    let materials = world.read_storage::<GpuMaterial>();
    let mut should_reload = world.write_storage::<ReloadMaterial>();
    let mut renderables = world.write_storage::<RenderableMaterial>();
    let entities = world.entities();

    for (ent, mesh, mat) in (&entities, &meshes, &materials).join() {
        let entry = renderables.entry(ent).expect("Failed to get entry!");
        match entry {
            StorageEntry::Occupied(mut entry) => {
                log::trace!("Using existing Renderable");
                if should_reload.contains(ent) {
                    log::trace!("Reloading shader for {:?}", ent);
                    // TODO: Destroy the previous pipeline
                    match get_pipeline_for(renderer, world, mesh, mat) {
                        Ok(pipeline) => entry.get_mut().set_pipeline(pipeline),
                        Err(e) => log::error!("Failed to compile pipeline: {}", e),
                    }
                }
            }
            StorageEntry::Vacant(entry) => {
                log::trace!("No Renderable found, creating new");
                let rend = create_renderable(renderer, world, mesh, mat);
                entry.insert(rend);
            }
        }
    }

    should_reload.clear();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrawMode {
    Lit,
    Unlit,
    ShadowsOnly,
}

#[profiling::function]
fn draw_entities<'a>(world: &World, cmd_buf: &mut RenderPassEncoder<'a>, mode: DrawMode) {
    let model_matrices = world.read_storage::<ModelMatrix>();
    let meshes = world.read_storage::<GpuMesh>();
    let renderables = world.read_storage::<RenderableMaterial>();
    use trekanten::pipeline::ShaderStage;

    let mut prev_handle: Option<Handle<GraphicsPipeline>> = None;

    // Only bind the pipeline if we need to
    let mut bind_pipeline = |enc: &mut RenderPassEncoder<'a>, handle: &Handle<GraphicsPipeline>| {
        if prev_handle.map(|h| h != *handle).unwrap_or(true) {
            enc.bind_graphics_pipeline(handle);
            prev_handle = Some(*handle);
        }
    };

    for (mesh, renderable, mtx) in (&meshes, &renderables, &model_matrices).join() {
        let tfm = uniform::Model {
            model: mtx.0.into_col_array(),
            model_it: mtx.0.inverted().transposed().into_col_array(),
        };

        match (renderable, mode) {
            (
                RenderableMaterial::PBR {
                    shadow_pipeline, ..
                },
                DrawMode::ShadowsOnly,
            ) => {
                bind_pipeline(cmd_buf, shadow_pipeline);
                cmd_buf
                    .bind_push_constant(shadow_pipeline, ShaderStage::VERTEX, &tfm)
                    .draw_mesh(&mesh.vertex_buffer, &mesh.index_buffer);
            }
            (
                RenderableMaterial::PBR {
                    gfx_pipeline,
                    material_descriptor_set,
                    ..
                },
                DrawMode::Lit,
            )
            | (
                RenderableMaterial::Unlit {
                    gfx_pipeline,
                    material_descriptor_set,
                },
                DrawMode::Unlit,
            ) => {
                bind_pipeline(cmd_buf, gfx_pipeline);
                cmd_buf
                    .bind_shader_resource_group(1, material_descriptor_set, gfx_pipeline)
                    .bind_push_constant(gfx_pipeline, ShaderStage::VERTEX, &tfm)
                    .draw_mesh(&mesh.vertex_buffer, &mesh.index_buffer);
            }
            _ => (),
        }
    }
}

#[profiling::function]
pub fn draw_frame(world: &mut World, ui: &mut ui::UIContext, renderer: &mut Renderer) {
    let cam_entity = ecs::find_singleton_entity::<Camera>(world);
    if cam_entity.is_none() {
        log::warn!("Did not find a camera entity, can't render");
        return;
    }

    GpuUpload::resolve_pending(world, renderer);
    create_renderables(renderer, world);

    let aspect_ratio = renderer.aspect_ratio();
    let mut frame = match renderer.next_frame() {
        frame @ Ok(_) => frame,
        Err(trekanten::RenderError::NeedsResize(reason)) => {
            log::debug!("Resize reason: {:?}", reason);
            renderer
                .resize(world.read_resource::<crate::io::MainWindow>().extents())
                .expect("Failed to resize renderer");
            renderer.next_frame()
        }
        e => e,
    }
    .expect("Failed to get next frame");

    let ui_draw_commands = ui.build_ui(world, &mut frame);

    let frame_resources = &*world.write_resource::<FrameData>();

    let cmd_buffer = frame
        .new_command_buffer()
        .expect("Failed to create command buffer");

    let mut cmd_buffer =
        light::light_and_shadow_pass(world, &mut frame, &frame_resources, cmd_buffer);

    // View data main render pass
    {
        let (view_matrix, view_pos) = get_view_data(world);
        let view_proj = get_proj_matrix(aspect_ratio) * view_matrix;
        let view_data = uniform::ViewData {
            view_proj: view_proj.into_col_array(),
            view_pos: [view_pos.x, view_pos.y, view_pos.z, 1.0f32],
        };

        frame
            .update_uniform_blocking(&frame_resources.main_camera_view_data, &view_data)
            .expect("Failed to update uniform");
    }

    {
        // main render pass
        let FrameData {
            main_render_pass,
            unlit_resources,
            pbr_resources,
            ..
        } = frame_resources;
        let mut main_rp = frame
            .begin_presentation_pass(cmd_buffer, main_render_pass)
            .expect("Failed to begin render pass");

        {
            let PhysicallyBasedUniformResources {
                dummy_pipeline,
                shader_resource_group,
                ..
            } = &pbr_resources;
            main_rp
                .bind_graphics_pipeline(dummy_pipeline)
                .bind_shader_resource_group(0u32, shader_resource_group, dummy_pipeline);
            draw_entities(world, &mut main_rp, DrawMode::Lit);
        }

        {
            let UnlitFrameUniformResources {
                dummy_pipeline,
                shader_resource_group,
            } = &unlit_resources;
            main_rp
                .bind_graphics_pipeline(dummy_pipeline)
                .bind_shader_resource_group(0u32, shader_resource_group, dummy_pipeline);
            draw_entities(world, &mut main_rp, DrawMode::Unlit);
        }

        if let Some(ui_draw_commands) = ui_draw_commands {
            ui_draw_commands.record_draw_commands(&mut main_rp);
        }

        cmd_buffer = main_rp.end().expect("Failed to end main presentation pass");
    }
    frame.add_command_buffer(cmd_buffer);

    let frame = frame.finish();
    renderer
        .submit(frame)
        .or_else(|e| {
            if let trekanten::RenderError::NeedsResize(reason) = e {
                log::info!("Resize reason: {:?}", reason);
                renderer.resize(world.read_resource::<crate::io::MainWindow>().extents())
            } else {
                Err(e)
            }
        })
        .expect("Failed to submit frame");
}

fn shadow_render_pass(renderer: &mut Renderer) -> Handle<trekanten::RenderPass> {
    use trekanten::raw_vk;
    let depth_attach = raw_vk::AttachmentDescription {
        format: raw_vk::Format::D16_UNORM,
        samples: raw_vk::SampleCountFlags::TYPE_1,
        load_op: raw_vk::AttachmentLoadOp::CLEAR,
        store_op: raw_vk::AttachmentStoreOp::STORE,
        stencil_load_op: raw_vk::AttachmentLoadOp::DONT_CARE,
        stencil_store_op: raw_vk::AttachmentStoreOp::DONT_CARE,
        initial_layout: raw_vk::ImageLayout::UNDEFINED,
        final_layout: raw_vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL,
        flags: raw_vk::AttachmentDescriptionFlags::empty(),
    };

    let depth_ref = raw_vk::AttachmentReference {
        attachment: 0,
        layout: raw_vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL,
    };

    let subpass = raw_vk::SubpassDescription::builder()
        .pipeline_bind_point(raw_vk::PipelineBindPoint::GRAPHICS)
        .depth_stencil_attachment(&depth_ref);

    // These subpass dependencies handle layout transistions, execution & memory dependencies
    // When there are multiple shadow passes, it might be valuable to use one pipeline barrier
    // for all of them instead of several subpass deps.
    let deps = [
        raw_vk::SubpassDependency {
            // The source pass deps here refer to the previous frame (I think :))
            src_subpass: raw_vk::SUBPASS_EXTERNAL,
            // Any previous fragment shader reads should be done
            src_stage_mask: raw_vk::PipelineStageFlags::FRAGMENT_SHADER,
            src_access_mask: raw_vk::AccessFlags::SHADER_READ,
            dst_subpass: 0,
            // EARLY_FRAGMENT_TESTS include subpass load operations for depth/stencil
            dst_stage_mask: raw_vk::PipelineStageFlags::EARLY_FRAGMENT_TESTS,
            // We are writing to the depth attachment
            dst_access_mask: raw_vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE,
            // We don't need a global dependency for the whole framebuffer
            dependency_flags: raw_vk::DependencyFlags::BY_REGION,
        },
        raw_vk::SubpassDependency {
            src_subpass: 0,
            // LATE_FRAGMENT_TESTS include subpass store operations for depth/stencil
            src_stage_mask: raw_vk::PipelineStageFlags::LATE_FRAGMENT_TESTS,
            src_access_mask: raw_vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE,

            // We want this render pass to complete before any subsequent uses of the depth buffer as a texture
            dst_subpass: raw_vk::SUBPASS_EXTERNAL,
            dst_stage_mask: raw_vk::PipelineStageFlags::FRAGMENT_SHADER,
            dst_access_mask: raw_vk::AccessFlags::SHADER_READ,
            // Do we actually need a full dependency region here?
            dependency_flags: raw_vk::DependencyFlags::empty(),
        },
    ];

    let attachments = [depth_attach];
    let subpasses = [subpass.build()];
    let create_info = raw_vk::RenderPassCreateInfo::builder()
        .attachments(&attachments)
        .subpasses(&subpasses)
        .dependencies(&deps);

    renderer
        .create_render_pass(&create_info)
        .expect("Failed to create shadow render pass")
}

// TODO: Runtime
const SHADOW_MAP_EXTENT: trekanten::util::Extent2D = trekanten::util::Extent2D {
    width: 1024,
    height: 1024,
};

fn shadow_render_target(
    renderer: &mut Renderer,
    render_pass: &Handle<trekanten::RenderPass>,
) -> (Handle<trekanten::Texture>, Handle<trekanten::RenderTarget>) {
    use trekanten::texture::{BorderColor, Filter, SamplerAddressMode};
    let extent = SHADOW_MAP_EXTENT;
    let format = util::Format::D16_UNORM;

    let desc = TextureDescriptor::Empty {
        extent,
        format,
        usage: TextureUsage::DEPTH_STENCIL_ATTACHMENT,
        sampler: SamplerDescriptor {
            filter: Filter::Linear,
            address_mode: SamplerAddressMode::ClampToEdge,
            max_anisotropy: None,
            border_color: BorderColor::FloatOpaqueWhite,
        },
    };
    let tex = renderer
        .create_texture(desc)
        .expect("Failed to create texture for shadow map");
    let attachments = [&tex];
    let render_target = renderer
        .create_render_target(render_pass, &attachments)
        .expect("Failed to create render target for shadow map");
    (tex, render_target)
}

fn build_shadow_data(
    shader_compiler: &pipeline::ShaderCompiler,
    renderer: &mut Renderer,
) -> ShadowData {
    use uniform::UniformBlock as _;

    let shadow_render_pass = shadow_render_pass(renderer);
    let view_data = vec![
        uniform::ViewData {
            view_proj: [0.0; 16],
            view_pos: [0.0; 4],
        };
        NUM_SPOTLIGHT_SHADOW_MAPS
    ];
    let view_data = OwningUniformBufferDescriptor::from_vec(view_data, BufferMutability::Mutable);
    let view_data_buffer_handles = renderer
        .create_resource_blocking(view_data)
        .expect("FAIL")
        .split();
    let spotlights: [SpotlightShadow; NUM_SPOTLIGHT_SHADOW_MAPS] = {
        let mut data: [MaybeUninit<SpotlightShadow>; NUM_SPOTLIGHT_SHADOW_MAPS] =
            unsafe { MaybeUninit::uninit().assume_init() };
        for i in 0..NUM_SPOTLIGHT_SHADOW_MAPS {
            let (texture, render_target) = shadow_render_target(renderer, &shadow_render_pass);
            let view_data_buffer = view_data_buffer_handles[i];
            let sh_view_data_set = DescriptorSet::builder(renderer)
                .add_buffer(
                    &view_data_buffer,
                    uniform::ViewData::BINDING,
                    trekanten::pipeline::ShaderStage::VERTEX,
                )
                .build();
            data[i] = MaybeUninit::new(SpotlightShadow {
                texture,
                render_target,
                view_data_buffer,
                view_data_desc_set: sh_view_data_set,
            });
        }
        unsafe { std::mem::transmute(data) }
    };
    let shadow_dummy_pipeline = {
        let pos_only_vertex_format = VertexFormat::builder()
            .add_attribute(util::Format::FLOAT3)
            .build();
        let pipeline_desc = shadow_pipeline_desc(&shader_compiler, pos_only_vertex_format)
            .expect("Failed to create graphics pipeline descriptor for shadows");
        renderer
            .create_gfx_pipeline(pipeline_desc, &shadow_render_pass)
            .expect("Failed to create pipeline for shadow")
    };

    ShadowData {
        render_pass: shadow_render_pass,
        dummy_pipeline: shadow_dummy_pipeline,
        spotlights,
    }
}

pub fn setup_resources(world: &mut World, mut renderer: &mut Renderer) {
    use trekanten::pipeline::ShaderStage;
    use uniform::UniformBlock as _;

    {
        let shader_compiler =
            pipeline::ShaderCompiler::new().expect("Failed to create shader compiler");

        world.insert(shader_compiler);
        world.insert(renderer.loader().unwrap());
    }

    let frame_data = {
        let shader_compiler = world.read_resource::<pipeline::ShaderCompiler>();

        log::trace!("Creating frame gpu resources");

        let main_render_pass = renderer
            .presentation_render_pass(8)
            .expect("main render pass creation failed");

        const N_VIEW_DATA: usize = 1;
        let view_data = vec![
            uniform::ViewData {
                view_proj: [0.0; 16],
                view_pos: [0.0; 4],
            };
            N_VIEW_DATA
        ];
        let view_data =
            OwningUniformBufferDescriptor::from_vec(view_data, BufferMutability::Mutable);
        let main_camera_view_data = renderer.create_resource_blocking(view_data).expect("FAIL");
        let shadow_data = build_shadow_data(&shader_compiler, renderer);

        let pbr_resources = {
            let vertex_format = VertexFormat::builder()
                .add_attribute(util::Format::FLOAT3)
                .add_attribute(util::Format::FLOAT3)
                .build();

            let result = pipeline::pbr_gltf::compile_default(&shader_compiler);
            let (vert, frag) = match result {
                Ok(r) => r,
                Err(e) => {
                    log::error!("{}", e);
                    return;
                }
            };

            let desc = GraphicsPipelineDescriptor::builder()
                .vert(ShaderDescriptor::FromRawSpirv(vert.data()))
                .frag(ShaderDescriptor::FromRawSpirv(frag.data()))
                .vertex_format(vertex_format)
                .build()
                .expect("Failed to build graphics pipeline descriptor");
            let dummy_pipeline = renderer
                .create_gfx_pipeline(desc, &main_render_pass)
                .expect("FAIL");

            // TODO: Single elem uniform buffer here. Add to the same buffer?
            let light_data = vec![uniform::LightingData {
                punctual_lights: [uniform::PackedLight::default(); uniform::MAX_NUM_LIGHTS],
                num_lights: 0,
                ambient: [0.0; 4],
            }];
            let light_data =
                OwningUniformBufferDescriptor::from_vec(light_data, BufferMutability::Mutable);
            let light_buffer = renderer.create_resource_blocking(light_data).expect("FAIL");

            let shadow_matrices = vec![uniform::ShadowMatrices {
                matrices: [uniform::Mat4::default(); uniform::MAX_NUM_LIGHTS],
                num_matrices: 0,
            }];
            let shadow_matrices =
                OwningUniformBufferDescriptor::from_vec(shadow_matrices, BufferMutability::Mutable);
            let shadow_matrices_buffer = renderer
                .create_resource_blocking(shadow_matrices)
                .expect("Failed to create shadow matrix uniform buffer");

            assert_eq!(uniform::LightingData::SET, uniform::ViewData::SET);
            let texture_itr = shadow_data.spotlights.iter().map(|x| (x.texture, true));
            let shader_resource_group = DescriptorSet::builder(&mut renderer)
                .add_buffer(
                    &main_camera_view_data,
                    uniform::ViewData::BINDING,
                    ShaderStage::VERTEX | ShaderStage::FRAGMENT,
                )
                .add_buffer(
                    &light_buffer,
                    uniform::LightingData::BINDING,
                    ShaderStage::FRAGMENT,
                )
                .add_textures(texture_itr, 2, ShaderStage::FRAGMENT)
                .add_buffer(&shadow_matrices_buffer, 3, ShaderStage::VERTEX)
                .build();

            PhysicallyBasedUniformResources {
                dummy_pipeline,
                light_buffer,
                shadow_matrices_buffer,
                shader_resource_group,
            }
        };

        let unlit_resources = {
            let shader_resource_group = DescriptorSet::builder(&mut renderer)
                .add_buffer(
                    &main_camera_view_data,
                    uniform::ViewData::BINDING,
                    ShaderStage::VERTEX,
                )
                .build();

            let vertex_format = VertexFormat::builder()
                .add_attribute(util::Format::FLOAT3)
                .build();
            let desc = unlit_pipeline_desc(
                &shader_compiler,
                vertex_format,
                trekanten::pipeline::PolygonMode::Line,
            )
            .expect("Failed to create descriptor for unlit dummy pipeline");
            let dummy_pipeline = renderer
                .create_gfx_pipeline(desc, &main_render_pass)
                .expect("Failed to create unlit dummy pipeline");

            UnlitFrameUniformResources {
                dummy_pipeline,
                shader_resource_group,
            }
        };

        FrameData {
            main_render_pass,
            main_camera_view_data,
            pbr_resources,
            unlit_resources,
            shadow: shadow_data,
        }
    };

    world.insert(frame_data);
    log::trace!("Done");
}

#[derive(Debug, Clone, Inspect)]
pub enum Pending<T1, T2> {
    Pending(T1),
    Available(T2),
}

struct GpuUpload;
impl GpuUpload {
    pub const ID: &'static str = "GpuUpload";
}
impl GpuUpload {
    #[profiling::function]
    fn resolve_pending(world: &mut World, renderer: &mut Renderer) {
        use trekanten::loader::HandleMapping;
        let mut loader = world.write_resource::<trekanten::Loader>();
        let mut pending_materials = world.write_storage::<PendingMaterial>();
        let mut pending_meshes = world.write_storage::<PendingMesh>();
        let mut materials = world.write_storage::<GpuMaterial>();
        let mut meshes = world.write_storage::<GpuMesh>();
        let mut transfer_guard = loader.transfer(renderer);
        let mut generate_mipmaps = Vec::new();
        for mapping in transfer_guard.iter() {
            match mapping {
                // TODO: drain_filter()
                HandleMapping::UniformBuffer { old, new } => {
                    for (ent, _) in (&world.entities(), &pending_materials.mask().clone()).join() {
                        if let Some(pending) = pending_materials.get_mut(ent) {
                            match pending {
                                PendingMaterial::Unlit { color_uniform } => match color_uniform {
                                    Pending::Pending(prev) if prev.handle() == old.handle() => {
                                        *color_uniform = Pending::Available(new);
                                    }
                                    _ => (),
                                },
                                PendingMaterial::PBR {
                                    material_uniforms, ..
                                } => match material_uniforms {
                                    Pending::Pending(prev) if prev.handle() == old.handle() => {
                                        *material_uniforms = Pending::Available(new);
                                    }
                                    _ => (),
                                },
                            }
                            if !pending.is_done() {
                                continue;
                            }

                            let material = pending_materials
                                .remove(ent)
                                .expect("This is alive")
                                .finish();
                            materials.insert(ent, material).expect("This is alive");
                        }
                    }
                }
                HandleMapping::VertexBuffer { old, new } => {
                    for (ent, _) in (&world.entities(), &pending_meshes.mask().clone()).join() {
                        if let Some(pending) = pending_meshes.get_mut(ent) {
                            match pending.vertex_buffer {
                                Pending::Pending(cur) if cur == old => {
                                    pending.vertex_buffer = Pending::Available(new);
                                }
                                _ => (),
                            }

                            // TODO: is_done + remove().finish() here
                            if let Some(mesh) = pending.try_finish() {
                                meshes.insert(ent, mesh).expect("I'm alive!");
                                pending_meshes.remove(ent).expect("I'm alive!");
                            }
                        }
                    }
                }
                HandleMapping::IndexBuffer { old, new } => {
                    for (ent, _) in (&world.entities(), &pending_meshes.mask().clone()).join() {
                        if let Some(pending) = pending_meshes.get_mut(ent) {
                            match pending.index_buffer {
                                Pending::Pending(cur) if cur == old => {
                                    pending.index_buffer = Pending::Available(new);
                                }
                                _ => (),
                            }

                            if let Some(mesh) = pending.try_finish() {
                                meshes.insert(ent, mesh).expect("I'm alive!");
                                pending_meshes.remove(ent).expect("I'm alive!");
                            }
                        }
                    }
                }
                HandleMapping::Texture { old, new } => {
                    for (ent, _) in (&world.entities(), &pending_materials.mask().clone()).join() {
                        if let Some(pending) = pending_materials.get_mut(ent) {
                            match pending {
                                PendingMaterial::PBR {
                                    normal_map,
                                    base_color_texture,
                                    metallic_roughness_texture,
                                    ..
                                } => {
                                    for tex in &mut [
                                        normal_map,
                                        base_color_texture,
                                        metallic_roughness_texture,
                                    ] {
                                        match tex {
                                            Some(Pending::Pending(tex_inner))
                                                if tex_inner.handle == old =>
                                            {
                                                generate_mipmaps.push(new);
                                                **tex = Some(Pending::Available(
                                                    material::TextureUse {
                                                        handle: new,
                                                        coord_set: tex_inner.coord_set,
                                                    },
                                                ));
                                            }
                                            _ => (),
                                        }
                                    }
                                }
                                PendingMaterial::Unlit { .. } => {
                                    unreachable!("Can't have pending textures for this variant")
                                }
                            };

                            if !pending.is_done() {
                                continue;
                            }

                            let material = pending_materials
                                .remove(ent)
                                .expect("This is alive")
                                .finish();

                            materials.insert(ent, material).expect("This is alive");
                        }
                    }
                }
            }
        }

        renderer
            .generate_mipmaps(&generate_mipmaps)
            .expect("Failed to generate mipmaps");
    }
}

impl<'a> System<'a> for GpuUpload {
    type SystemData = (
        WriteExpect<'a, trekanten::Loader>,
        WriteStorage<'a, material::Unlit>,
        WriteStorage<'a, material::PhysicallyBased>,
        WriteStorage<'a, PendingMaterial>,
        WriteStorage<'a, GpuMaterial>,
        WriteStorage<'a, mesh::CpuMesh>,
        WriteStorage<'a, PendingMesh>,
        WriteStorage<'a, mesh::GpuMesh>,
        Entities<'a>,
    );

    fn run(&mut self, data: Self::SystemData) {
        use trekanten::loader::ResourceLoader;

        let (
            loader,
            unlit_materials,
            physically_based_materials,
            mut pending_mats,
            gpu_materials,
            cpu_meshes,
            mut pending_meshes,
            gpu_meshes,
            entities,
        ) = data;

        {
            // Unlit
            let mut ubuf = Vec::new();
            for (_, unlit, _, _) in
                (&entities, &unlit_materials, !&gpu_materials, !&pending_mats).join()
            {
                ubuf.push(uniform::UnlitUniformData {
                    color: unlit.color.into_array(),
                });
            }

            if !ubuf.is_empty() {
                let async_handle = loader
                    .load(OwningUniformBufferDescriptor::from_vec(
                        ubuf,
                        BufferMutability::Immutable,
                    ))
                    .expect("Failed to load uniform buffer");
                for (i, (ent, _unlit, _)) in (&entities, &unlit_materials, !&gpu_materials)
                    .join()
                    .enumerate()
                {
                    if let StorageEntry::Vacant(entry) = pending_mats.entry(ent).unwrap() {
                        entry.insert(PendingMaterial::Unlit {
                            color_uniform: Pending::Pending(BufferHandle::sub_buffer(
                                async_handle,
                                i as u32,
                                1,
                            )),
                        });
                    }
                }
            }
        }

        {
            // Physically based
            let mut ubuf_pbr = Vec::new();
            for (_, pb_mat, _, _) in (
                &entities,
                &physically_based_materials,
                !&gpu_materials,
                !&pending_mats,
            )
                .join()
            {
                ubuf_pbr.push(uniform::PBRMaterialData {
                    base_color_factor: pb_mat.base_color_factor.into_array(),
                    metallic_factor: pb_mat.metallic_factor,
                    roughness_factor: pb_mat.roughness_factor,
                    normal_scale: pb_mat.normal_scale,
                    _padding: 0.0,
                });
            }

            let map_tex = |inp: &Option<material::TextureUse2>| -> Option<
                Pending<
                    material::TextureUse<resurs::Async<trekanten::texture::Texture>>,
                    material::TextureUse<trekanten::texture::Texture>,
                >,
            > {
                inp.as_ref().map(|tex| {
                    let handle = loader
                        .load(tex.desc.clone())
                        .expect("Failed to load texture");
                    Pending::Pending(material::TextureUse {
                        coord_set: tex.coord_set,
                        handle,
                    })
                })
            };

            if !ubuf_pbr.is_empty() {
                let async_handle = loader
                    .load(OwningUniformBufferDescriptor::from_vec(
                        ubuf_pbr,
                        BufferMutability::Immutable,
                    ))
                    .expect("Failed to load uniform buffer");
                for (i, (ent, pb_mat, _)) in
                    (&entities, &physically_based_materials, !&gpu_materials)
                        .join()
                        .enumerate()
                {
                    if let StorageEntry::Vacant(entry) = pending_mats.entry(ent).unwrap() {
                        entry.insert(PendingMaterial::PBR {
                            material_uniforms: Pending::Pending(BufferHandle::sub_buffer(
                                async_handle,
                                i as u32,
                                1,
                            )),
                            normal_map: map_tex(&pb_mat.normal_map),
                            base_color_texture: map_tex(&pb_mat.base_color_texture),
                            metallic_roughness_texture: map_tex(&pb_mat.metallic_roughness_texture),
                            has_vertex_colors: pb_mat.has_vertex_colors,
                        });
                    }
                }
            }
        }

        for (ent, mesh, _) in (&entities, &cpu_meshes, !&gpu_meshes).join() {
            if let StorageEntry::Vacant(entry) = pending_meshes.entry(ent).unwrap() {
                entry.insert(PendingMesh::load(&loader, &mesh));
            }
        }
    }
}

pub fn register_systems<'a, 'b>(builder: ExecutorBuilder<'a, 'b>) -> ExecutorBuilder<'a, 'b> {
    register_module_systems!(builder, debug_window, bounding_box, light).with(
        GpuUpload,
        GpuUpload::ID,
        &[],
    )
}
