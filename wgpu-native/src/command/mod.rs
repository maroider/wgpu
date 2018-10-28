mod allocator;
mod compute;
mod render;

pub use self::allocator::CommandAllocator;
pub use self::compute::*;
pub use self::render::*;

use hal::{self, Device};
use hal::command::RawCommandBuffer;

use {
    B, Color, Origin3d, Stored, BufferUsageFlags, TextureUsageFlags,
    BufferId, CommandBufferId, ComputePassId, DeviceId, RenderPassId, TextureId, TextureViewId,
};
use conv;
use registry::{HUB, Items, Registry};
use track::{BufferTracker, TextureTracker};

use std::iter;
use std::ops::Range;
use std::thread::ThreadId;


#[repr(C)]
#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
pub enum LoadOp {
    Clear = 0,
    Load = 1,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
pub enum StoreOp {
    Store = 0,
}

#[repr(C)]
pub struct RenderPassColorAttachmentDescriptor<T> {
    pub attachment: T,
    pub load_op: LoadOp,
    pub store_op: StoreOp,
    pub clear_color: Color,
}

#[repr(C)]
pub struct RenderPassDepthStencilAttachmentDescriptor<T> {
    pub attachment: T,
    pub depth_load_op: LoadOp,
    pub depth_store_op: StoreOp,
    pub clear_depth: f32,
    pub stencil_load_op: LoadOp,
    pub stencil_store_op: StoreOp,
    pub clear_stencil: u32,
}

#[repr(C)]
pub struct RenderPassDescriptor<'a, T: 'a> {
    pub color_attachments: &'a [RenderPassColorAttachmentDescriptor<T>],
    pub depth_stencil_attachment: Option<RenderPassDepthStencilAttachmentDescriptor<T>>,
}

#[repr(C)]
pub struct BufferCopyView {
    pub buffer: BufferId,
    pub offset: u32,
    pub row_pitch: u32,
    pub image_height: u32,
}

#[repr(C)]
pub struct TextureCopyView {
    pub texture: TextureId,
    pub level: u32,
    pub slice: u32,
    pub origin: Origin3d,
    //TODO: pub aspect: TextureAspect,
}

pub struct CommandBuffer<B: hal::Backend> {
    pub(crate) raw: Vec<B::CommandBuffer>,
    fence: B::Fence,
    recorded_thread_id: ThreadId,
    device_id: Stored<DeviceId>,
    pub(crate) buffer_tracker: BufferTracker,
    pub(crate) texture_tracker: TextureTracker,
}

impl CommandBuffer<B> {
    pub(crate) fn insert_barriers<I, J>(
        raw: &mut <B as hal::Backend>::CommandBuffer,
        buffer_iter: I,
        texture_iter: J,
    ) where
        I: Iterator<Item = (BufferId, Range<BufferUsageFlags>)>,
        J: Iterator<Item = (TextureId, Range<TextureUsageFlags>)>,
    {
        let buffer_guard = HUB.buffers.lock();
        let texture_guard = HUB.textures.lock();

        let buffer_barriers = buffer_iter.map(|(id, transit)| {
            let b = buffer_guard.get(id);
            trace!("transit {:?} {:?}", id, transit);
            hal::memory::Barrier::Buffer {
                states: conv::map_buffer_state(transit.start) ..
                    conv::map_buffer_state(transit.end),
                target: &b.raw,
            }
        });
        let texture_barriers = texture_iter.map(|(id, transit)| {
            let t = texture_guard.get(id);
            trace!("transit {:?} {:?}", id, transit);
            let aspects = t.full_range.aspects;
            hal::memory::Barrier::Image {
                states: conv::map_texture_state(transit.start, aspects) ..
                    conv::map_texture_state(transit.end, aspects),
                target: &t.raw,
                range: t.full_range.clone(), //TODO?
            }
        });

        raw.pipeline_barrier(
            hal::pso::PipelineStage::TOP_OF_PIPE .. hal::pso::PipelineStage::BOTTOM_OF_PIPE,
            hal::memory::Dependencies::empty(),
            buffer_barriers.chain(texture_barriers),
        );
    }
}

#[repr(C)]
pub struct CommandBufferDescriptor {}

#[no_mangle]
pub extern "C" fn wgpu_command_buffer_begin_render_pass(
    command_buffer_id: CommandBufferId,
    desc: RenderPassDescriptor<TextureViewId>,
) -> RenderPassId {
    let mut cmb_guard = HUB.command_buffers.lock();
    let cmb = cmb_guard.get_mut(command_buffer_id);
    let device_guard = HUB.devices.lock();
    let device = device_guard.get(cmb.device_id.0);
    let view_guard = HUB.texture_views.lock();

    let mut current_comb = device.com_allocator.extend(cmb);
    current_comb.begin(
        hal::command::CommandBufferFlags::ONE_TIME_SUBMIT,
        hal::command::CommandBufferInheritanceInfo::default(),
    );
    let mut extent = None;

    let render_pass = {
        let tracker = &mut cmb.texture_tracker;

        let depth_stencil_attachment = match desc.depth_stencil_attachment {
            Some(ref at) => {
                let view = view_guard.get(at.attachment);
                if let Some(ex) = extent {
                    assert_eq!(ex, view.extent);
                } else {
                    extent = Some(view.extent);
                }
                let query = tracker.query(view.texture_id.0, TextureUsageFlags::empty());
                let (_, layout) = conv::map_texture_state(
                    query.usage,
                    hal::format::Aspects::DEPTH | hal::format::Aspects::STENCIL,
                );
                Some(hal::pass::Attachment {
                    format: Some(conv::map_texture_format(view.format)),
                    samples: view.samples,
                    ops: conv::map_load_store_ops(at.depth_load_op, at.depth_store_op),
                    stencil_ops: conv::map_load_store_ops(at.stencil_load_op, at.stencil_store_op),
                    layouts: layout .. layout,
                })
            }
            None => None,
        };
        let color_attachments = desc.color_attachments
            .iter()
            .map(|at| {
                let view = view_guard.get(at.attachment);
                if let Some(ex) = extent {
                    assert_eq!(ex, view.extent);
                } else {
                    extent = Some(view.extent);
                }
                let query = tracker.query(view.texture_id.0, TextureUsageFlags::empty());
                let (_, layout) = conv::map_texture_state(query.usage, hal::format::Aspects::COLOR);
                hal::pass::Attachment {
                    format: Some(conv::map_texture_format(view.format)),
                    samples: view.samples,
                    ops: conv::map_load_store_ops(at.load_op, at.store_op),
                    stencil_ops: hal::pass::AttachmentOps::DONT_CARE,
                    layouts: layout .. layout,
                }
            });
        let attachments = color_attachments.chain(depth_stencil_attachment);

        //TODO: retain the storage
        let color_refs = (0 .. desc.color_attachments.len())
            .map(|i| {
                (i, hal::image::Layout::ColorAttachmentOptimal)
            })
            .collect::<Vec<_>>();
        let ds_ref = desc.depth_stencil_attachment.as_ref().map(|_| {
            (desc.color_attachments.len(), hal::image::Layout::DepthStencilAttachmentOptimal)
        });
        let subpass = hal::pass::SubpassDesc {
            colors: &color_refs,
            depth_stencil: ds_ref.as_ref(),
            inputs: &[],
            resolves: &[],
            preserves: &[],
        };

        device.raw.create_render_pass(attachments, iter::once(subpass), &[])
    };

    let framebuffer = {
        let attachments = desc.color_attachments
            .iter()
            .map(|at| at.attachment)
            .chain(desc.depth_stencil_attachment.as_ref().map(|at| at.attachment))
            .map(|id| &view_guard.get(id).raw);
        device.raw
            .create_framebuffer(&render_pass, attachments, extent.unwrap())
            .unwrap()
    };

    let rect = {
        let ex = extent.unwrap();
        hal::pso::Rect {
            x: 0,
            y: 0,
            w: ex.width as _,
            h: ex.height as _,
        }
    };
    let clear_values = desc.color_attachments
        .iter()
        .map(|at| {
            //TODO: integer types?
            let value = hal::command::ClearColor::Float(conv::map_color(at.clear_color));
            hal::command::ClearValueRaw::from(hal::command::ClearValue::Color(value))
        })
        .chain(desc.depth_stencil_attachment.map(|at| {
            let value = hal::command::ClearDepthStencil(at.clear_depth, at.clear_stencil);
            hal::command::ClearValueRaw::from(hal::command::ClearValue::DepthStencil(value))
        }));
    current_comb.begin_render_pass(
        &render_pass,
        &framebuffer,
        rect,
        clear_values,
        hal::command::SubpassContents::Inline,
    );

    HUB.render_passes
        .lock()
        .register(RenderPass::new(
            current_comb,
            command_buffer_id,
        ))
}

#[no_mangle]
pub extern "C" fn wgpu_command_buffer_begin_compute_pass(
    command_buffer_id: CommandBufferId,
) -> ComputePassId {
    let mut cmb_guard = HUB.command_buffers.lock();
    let cmb = cmb_guard.get_mut(command_buffer_id);

    let raw = cmb.raw.pop().unwrap();

    HUB.compute_passes
        .lock()
        .register(ComputePass::new(raw, command_buffer_id))
}