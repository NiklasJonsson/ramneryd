#[macro_use]
extern crate vulkano;
#[macro_use]
extern crate num_derive;
#[macro_use]
extern crate specs_derive;
extern crate clap;
extern crate nalgebra_glm as glm;

use vulkano_win::VkSurfaceBuild;

use winit::{Event, EventsLoop, WindowBuilder, WindowEvent};

use specs::prelude::*;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

mod asset;
mod camera;
mod common;
mod game_state;
mod input;
mod render;
mod settings;

use self::asset::AssetDescriptor;
use self::common::*;
use self::render::*;

use self::game_state::GameState;

#[derive(Default, Debug, Copy, Clone)]
pub struct DeltaTime(Duration);

impl DeltaTime {
    pub fn zero() -> DeltaTime {
        DeltaTime(Duration::new(0, 0))
    }

    pub fn to_f32(self) -> f32 {
        (self.0.as_secs() as f64 + self.0.subsec_nanos() as f64 / 1_000_000_000.0) as f32
    }

    pub fn as_fps(self) -> f32 {
        1.0 / self.to_f32()
    }
}

impl Into<Duration> for DeltaTime {
    fn into(self) -> Duration {
        self.0
    }
}

impl From<Duration> for DeltaTime {
    fn from(dur: Duration) -> Self {
        DeltaTime(dur)
    }
}

impl std::ops::Mul<f32> for DeltaTime {
    type Output = f32;
    fn mul(self, other: f32) -> Self::Output {
        self.to_f32() * other
    }
}

struct App {
    world: World,
    events_loop: EventsLoop,
    vk_manager: VKManager,
}

// TODO: Handle resized here as well
#[derive(Debug)]
enum AppAction {
    Quit,
    IgnoreInput,
    AcceptInput(Vec<input::ExternalInput>),
    HandleEvents(Vec<input::ExternalInput>),
}

impl AppAction {
    fn update_with(self, new: Self) -> Self {
        use AppAction::*;
        match (new, self) {
            (_, Quit) => Quit,
            (_, IgnoreInput) => IgnoreInput,
            (Quit, _) => Quit,
            (IgnoreInput, _) => IgnoreInput,
            (AcceptInput(vec), _) => AcceptInput(vec),
            (HandleEvents(mut new_events), AcceptInput(mut old_events)) => {
                old_events.append(&mut new_events);
                AcceptInput(old_events)
            }
            (HandleEvents(mut new_events), HandleEvents(mut old_events)) => {
                old_events.append(&mut new_events);
                HandleEvents(old_events)
            }
        }
    }
}

struct EventManager {
    action: AppAction,
}

// TODO: We should not have "ignore-code" in both event manager and input manager
// Only stuff that is relevant to input manager should be forwarded.
// Create enum to represent what we want the input manager to receive
// But should this really be done here? Separate window/input handling?
// Move this to input? IOManager? Use Channels to propagate info instead of resource?
impl EventManager {
    fn new() -> Self {
        Self {
            action: AppAction::HandleEvents(Vec::new()),
        }
    }

    fn update_action(&mut self, action: AppAction) {
        let cur = std::mem::replace(&mut self.action, AppAction::Quit);
        self.action = cur.update_with(action);
    }

    fn collect_event(&mut self, event: Event) {
        use input::ExternalInput as ExtInp;
        match event {
            Event::WindowEvent {
                event: inner_event, ..
            } => match inner_event {
                WindowEvent::CloseRequested => {
                    log::info!("EventManager: Received CloseRequested window event");
                    self.update_action(AppAction::Quit);
                }
                WindowEvent::Focused(false) => {
                    log::trace!("Window lost focus, ignoring input");
                    self.update_action(AppAction::IgnoreInput);
                }
                WindowEvent::Focused(true) => {
                    log::trace!("Window gained focus, accepting input");
                    self.update_action(AppAction::AcceptInput(Vec::new()));
                }
                WindowEvent::KeyboardInput { device_id, input } => {
                    let is_pressed = input.state == winit::ElementState::Pressed;
                    if let Some(key) = input.virtual_keycode {
                        let ei = if is_pressed {
                            input::ExternalInput::KeyPress(key)
                        } else {
                            input::ExternalInput::KeyRelease(key)
                        };

                        self.update_action(AppAction::HandleEvents(vec![ei]));
                    } else {
                        log::warn!("Key clicked but no virtual key mapped!");
                    }
                }
                e => log::trace!("Igoring window event {:?}", e),
            },
            Event::DeviceEvent {
                event: inner_event, ..
            } => {
                if let winit::DeviceEvent::MouseMotion { delta: (x, y) } = inner_event {
                    let ei = input::ExternalInput::MouseDelta { x, y };
                    self.update_action(AppAction::HandleEvents(vec![ei]));
                } else {
                    log::trace!("Igoring device event {:?}", inner_event);
                }
            }
            e => log::trace!("Igoring event {:?}", e),
        };
    }

    fn resolve(&mut self) -> AppAction {
        std::mem::replace(&mut self.action, AppAction::HandleEvents(Vec::new()))
    }
}

struct Args {
    gltf_path: PathBuf,
    use_scene_camera: bool,
    run_n_frames: Option<usize>,
}

impl App {
    // FIXME: This lives here only because the lifetime parameters are a pain.
    // The whole App struct would need to be templated if this was included.
    // Maybe this can be solved in another way...
    fn init_dispatcher<'a, 'b>() -> Dispatcher<'a, 'b> {
        let builder = DispatcherBuilder::new();
        // Input needs to go before as most systems depends on it
        let builder = input::register_systems(builder);

        let builder = camera::register_systems(builder);
        let builder = settings::register_systems(builder);
        let builder = game_state::register_systems(builder);

        builder
            .with_barrier()
            .with(
                render_graph::TransformPropagation,
                "transform_propagation",
                &[],
            )
            .build()
    }

    fn setup_resources(&mut self) {
        self.world
            .insert(input::CurrentFrameExternalInputs(Vec::new()));
        self.world.insert(ActiveCamera::empty());
        self.world.insert(DeltaTime::zero());
    }

    pub fn get_entity_with_marker<C>(w: &World) -> Entity
    where
        C: specs::Component,
    {
        let markers = w.read_storage::<C>();
        let entities = w.read_resource::<specs::world::EntitiesRes>();

        let mut joined = (&entities, &markers).join();
        let item = joined.next();
        assert!(
            joined.next().is_none(),
            "Expected only one entity with marker component!"
        );
        let (ent, _) = item.expect("Expected an entity!");

        ent
    }

    // TODO: Move this
    pub fn entity_has_component<C>(w: &World, e: Entity) -> bool
    where
        C: specs::Component,
    {
        w.read_storage::<C>().get(e).is_some()
    }

    fn populate_world(&mut self, args: &Args) {
        self.setup_resources();

        let cam_entity = Self::get_entity_with_marker::<camera::Camera>(&self.world);
        *self.world.write_resource::<ActiveCamera>() = ActiveCamera::with_entity(cam_entity);

        let desc = AssetDescriptor::Gltf {
            path: args.gltf_path.to_owned(),
        };

        let loaded_asset = asset::load_asset_into(&mut self.world, desc);
        // REFACTOR: Flatten this when support for && and if-let is on stable
        if args.use_scene_camera {
            if let Some(transform) = loaded_asset.camera {
                camera::Camera::set_camera_state(&mut self.world, cam_entity, &transform);
            }
        }
    }

    fn run(&mut self, args: Args) {
        let mut dispatcher = Self::init_dispatcher();

        // Register all component types
        self.world.register::<Renderable>();
        self.world.register::<GraphicsPrimitive>();
        self.world.register::<render_graph::RenderGraphNode>();
        self.world.register::<render_graph::RenderGraphRoot>();
        self.world.register::<render_graph::RenderGraphChild>();
        self.world.register::<camera::Camera>();
        dispatcher.setup(&mut self.world);

        // Setup world objects, e.g. camera and chalet model
        self.populate_world(&args);

        // Collects events and resolves to AppAction
        let mut event_manager = EventManager::new();

        let _start_time = Instant::now();
        let mut prev_frame = Instant::now();

        let mut frame_count = 0;

        // Main loop is structured like:
        // 1. Poll events
        // 2. Resolve events
        // 3. Grab/release cursor
        // 4. Acquire swapchain image
        // 5. Wait for previous frame
        // 6. Run logic systems
        // 7. Render
        loop {
            // Update global delta time
            let now = Instant::now();
            let diff = now - prev_frame;
            prev_frame = now;

            *self.world.write_resource::<DeltaTime>() = diff.into();

            self.events_loop
                .poll_events(|event| event_manager.collect_event(event));

            match event_manager.resolve() {
                AppAction::Quit => {
                    return;
                }
                AppAction::IgnoreInput => {
                    continue;
                }
                AppAction::AcceptInput(events) | AppAction::HandleEvents(events) => {
                    let mut cur_events = self
                        .world
                        .write_resource::<input::CurrentFrameExternalInputs>();
                    *cur_events = input::CurrentFrameExternalInputs(events);
                }
            }

            let grab_cursor = *self.world.read_resource::<GameState>() == GameState::Running;

            if grab_cursor {
                self.vk_manager.take_cursor();
            } else {
                self.vk_manager.release_cursor();
            }

            // Acquires next swapchain frame and waits for previous work to the upcoming framebuffer to be finished.
            self.vk_manager.prepare_frame();

            // Run all ECS systems (blocking call)
            dispatcher.dispatch(&self.world);

            // Send data to GPU
            self.vk_manager
                .prepare_primitives_for_rendering(&self.world);

            // Run render systems, this is done after the dispatch call to enforce serialization
            self.vk_manager.draw_next_frame(&mut self.world);

            frame_count += 1;
            if let Some(n_frames) = args.run_n_frames {
                assert!(frame_count <= n_frames);
                if frame_count == n_frames {
                    break;
                }
            }
        }
    }

    fn new() -> Self {
        let vk_instance = render::get_vk_instance();

        let events_loop = EventsLoop::new();
        let vk_surface = WindowBuilder::new()
            .build_vk_surface(&events_loop, Arc::clone(&vk_instance))
            .expect("Unable to create window/surface");

        let world = World::new();

        let vk_manager = VKManager::create(vk_instance, vk_surface);

        App {
            world,
            events_loop,
            vk_manager,
        }
    }
}

fn main() {
    env_logger::init();
    let matches = clap::App::new("ramneryd")
        .version("0.1.0")
        .about("Vulkan renderer")
        .arg(
            clap::Arg::with_name("view-gltf")
                .short("-i")
                .long("view-gltf")
                .value_name("GLTF-FILE")
                .help("Reads a gltf file and renders it.")
                .takes_value(true)
                .required(true),
        )
        // TODO: This can only be used if we are passing a scene from the command line
        .arg(
            clap::Arg::with_name("use-scene-camera")
                .long("use-scene-camera")
                .help("Use the camera encoded in e.g. a gltf scene"),
        )
        .arg(
            clap::Arg::with_name("run-n-frames")
                .long("run-n-frames")
                .value_name("N")
                .takes_value(true)
                .help("Run only N frames"),
        )
        .get_matches();

    let path = matches.value_of("view-gltf").expect("This is required!");
    let path_buf = PathBuf::from(path);
    if !path_buf.exists() {
        println!("No such path: {}!", path_buf.as_path().display());
        return;
    }

    let use_scene_camera = matches.is_present("use-scene-camera");

    let run_n_frames = if let Some(s) = matches.value_of("run-n-frames") {
        match s.parse::<usize>() {
            Ok(n) => Some(n),
            Err(e) => {
                println!("Invalid value for run-n-frames: {}", e);
                return;
            }
        }
    } else {
        None
    };

    let args = Args {
        gltf_path: path_buf,
        use_scene_camera,
        run_n_frames,
    };

    let mut app = App::new();

    app.run(args);
}
