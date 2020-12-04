use ash::vk;

use crate::command::CommandBuffer;
use crate::device::Device;
use crate::mem;
use crate::mem::{BufferDescriptor, BufferHandle, BufferResult, MemoryError};
use crate::util::{as_byte_slice, ByteBuffer};
use crate::vertex::VertexDefinition;
use crate::vertex::VertexFormat;

pub use crate::mem::BufferMutability;

use std::sync::Arc;

pub struct Mesh {
    pub vertex_buffer: BufferHandle<VertexBuffer>,
    pub index_buffer: BufferHandle<IndexBuffer>,
}

#[derive(Debug, Copy, Clone)]
pub enum IndexSize {
    Size32,
    Size16,
}

impl From<IndexSize> for vk::IndexType {
    fn from(is: IndexSize) -> Self {
        match is {
            IndexSize::Size16 => vk::IndexType::UINT16,
            IndexSize::Size32 => vk::IndexType::UINT32,
        }
    }
}

impl IndexSize {
    fn size(&self) -> u8 {
        match self {
            Self::Size32 => 4,
            Self::Size16 => 2,
        }
    }
}

pub struct IndexBufferDescriptor<'a> {
    data: &'a [u8],
    index_size: IndexSize,
    mutability: BufferMutability,
}

impl<'a> IndexBufferDescriptor<'a> {
    pub fn from_slice<T: num_traits::PrimInt>(
        slice: &'a [T],
        mutability: BufferMutability,
    ) -> Self {
        let data = as_byte_slice(slice);
        let index_size = match std::mem::size_of::<T>() {
            4 => IndexSize::Size32,
            2 => IndexSize::Size16,
            _ => unreachable!("Invalid index type, needs to be either 16 or 32 bits"),
        };

        Self {
            data,
            index_size,
            mutability,
        }
    }
}

impl<'a> BufferDescriptor for IndexBufferDescriptor<'a> {
    type Buffer = IndexBuffer;

    fn mutability(&self) -> BufferMutability {
        self.mutability
    }

    fn elem_size(&self) -> u16 {
        self.index_size.size() as u16
    }

    fn n_elems(&self) -> u32 {
        assert_eq!(self.data.len() % self.elem_size() as usize, 0);
        (self.data.len() / self.elem_size() as usize) as u32
    }

    fn stride(&self, _: &Device) -> u16 {
        self.elem_size()
    }

    fn vk_usage_flags(&self) -> vk::BufferUsageFlags {
        vk::BufferUsageFlags::INDEX_BUFFER
    }

    fn data(&self) -> &[u8] {
        self.data
    }

    fn enqueue_single(
        &self,
        device: &Device,
        command_buffer: &mut CommandBuffer,
    ) -> Result<BufferResult<Self::Buffer>, MemoryError> {
        let (buffer, transient) = Self::Buffer::create(
            device,
            command_buffer,
            self,
            IndexBufferType {
                index_size: self.index_size,
            },
        )?;

        Ok(BufferResult { buffer, transient })
    }
}

#[derive(Clone)]
pub struct OwningIndexBufferDescriptor {
    data: Arc<ByteBuffer>,
    index_size: IndexSize,
    mutability: BufferMutability,
}

impl OwningIndexBufferDescriptor {
    pub fn from_vec<T: num_traits::PrimInt>(data: Vec<T>, mutability: BufferMutability) -> Self {
        let index_size = match std::mem::size_of::<T>() {
            4 => IndexSize::Size32,
            2 => IndexSize::Size16,
            _ => unreachable!("Invalid index type, needs to be either 16 or 32 bits"),
        };
        let data = unsafe { Arc::new(ByteBuffer::from_vec(data)) };
        Self {
            data,
            index_size,
            mutability,
        }
    }
}

impl BufferDescriptor for OwningIndexBufferDescriptor {
    type Buffer = IndexBuffer;

    fn mutability(&self) -> BufferMutability {
        self.mutability
    }

    fn elem_size(&self) -> u16 {
        self.index_size.size() as u16
    }

    fn n_elems(&self) -> u32 {
        assert_eq!(self.data.len() % self.elem_size() as usize, 0);
        (self.data.len() / self.elem_size() as usize) as u32
    }

    fn stride(&self, _: &Device) -> u16 {
        self.elem_size()
    }

    fn vk_usage_flags(&self) -> vk::BufferUsageFlags {
        vk::BufferUsageFlags::INDEX_BUFFER
    }

    fn data(&self) -> &[u8] {
        &self.data
    }

    fn enqueue_single(
        &self,
        device: &Device,
        command_buffer: &mut CommandBuffer,
    ) -> Result<BufferResult<Self::Buffer>, MemoryError> {
        let (buffer, transient) = Self::Buffer::create(
            device,
            command_buffer,
            self,
            IndexBufferType {
                index_size: self.index_size,
            },
        )?;

        Ok(BufferResult { buffer, transient })
    }
}

#[derive(Debug)]
pub struct IndexBufferType {
    index_size: IndexSize,
}

pub type IndexBuffer = mem::TypedBuffer<IndexBufferType>;

impl IndexBuffer {
    pub fn vk_index_type(&self) -> vk::IndexType {
        vk::IndexType::from(self.buffer_type().index_size)
    }
}

#[derive(Debug)]
pub struct VertexBufferType {
    format: VertexFormat,
}

pub type VertexBuffer = mem::TypedBuffer<VertexBufferType>;

impl VertexBuffer {
    pub fn format(&self) -> &VertexFormat {
        &self.buffer_type().format
    }
}

#[derive(Clone)]
pub struct OwningVertexBufferDescriptor {
    data: Arc<ByteBuffer>,
    format: VertexFormat,
    mutability: BufferMutability,
}

impl OwningVertexBufferDescriptor {
    pub fn from_vec<V: VertexDefinition + Copy>(
        data: Vec<V>,
        mutability: BufferMutability,
    ) -> Self {
        let data = unsafe { Arc::new(ByteBuffer::from_vec(data)) };
        Self {
            data,
            format: V::format(),
            mutability,
        }
    }
}

impl<'a> BufferDescriptor for OwningVertexBufferDescriptor {
    type Buffer = VertexBuffer;
    fn mutability(&self) -> BufferMutability {
        self.mutability
    }

    fn elem_size(&self) -> u16 {
        self.format.size() as u16
    }

    fn n_elems(&self) -> u32 {
        assert_eq!(self.data.len() % self.elem_size() as usize, 0);
        (self.data.len() / self.elem_size() as usize) as u32
    }

    fn stride(&self, _: &Device) -> u16 {
        self.elem_size()
    }

    fn data(&self) -> &[u8] {
        &self.data
    }

    fn vk_usage_flags(&self) -> vk::BufferUsageFlags {
        vk::BufferUsageFlags::VERTEX_BUFFER
    }
    fn enqueue_single(
        &self,
        device: &Device,
        command_buffer: &mut CommandBuffer,
    ) -> Result<BufferResult<Self::Buffer>, MemoryError> {
        let (buffer, transient) = Self::Buffer::create(
            device,
            command_buffer,
            self,
            VertexBufferType {
                format: self.format.clone(),
            },
        )?;

        Ok(BufferResult { buffer, transient })
    }
}

pub struct VertexBufferDescriptor<'a> {
    data: &'a [u8],
    format: VertexFormat,
    mutability: BufferMutability,
}

impl<'a> VertexBufferDescriptor<'a> {
    pub fn from_slice<V: VertexDefinition + Copy>(
        slice: &'a [V],
        mutability: BufferMutability,
    ) -> Self {
        let data = as_byte_slice(slice);
        Self {
            data,
            format: V::format(),
            mutability,
        }
    }

    pub fn from_raw(data: &'a [u8], format: VertexFormat, mutability: BufferMutability) -> Self {
        Self {
            data,
            format,
            mutability,
        }
    }
}

impl<'a> BufferDescriptor for VertexBufferDescriptor<'a> {
    type Buffer = VertexBuffer;
    fn mutability(&self) -> BufferMutability {
        self.mutability
    }

    fn elem_size(&self) -> u16 {
        self.format.size() as u16
    }

    fn n_elems(&self) -> u32 {
        assert_eq!(self.data.len() % self.elem_size() as usize, 0);
        (self.data.len() / self.elem_size() as usize) as u32
    }

    fn stride(&self, _: &Device) -> u16 {
        self.elem_size()
    }

    fn data(&self) -> &[u8] {
        self.data
    }

    fn vk_usage_flags(&self) -> vk::BufferUsageFlags {
        vk::BufferUsageFlags::VERTEX_BUFFER
    }

    fn enqueue_single(
        &self,
        device: &Device,
        command_buffer: &mut CommandBuffer,
    ) -> Result<BufferResult<Self::Buffer>, MemoryError> {
        let (buffer, transient) = Self::Buffer::create(
            device,
            command_buffer,
            self,
            VertexBufferType {
                format: self.format.clone(),
            },
        )?;

        Ok(BufferResult { buffer, transient })
    }
}

pub type VertexBuffers = mem::DeviceBufferStorage<VertexBuffer>;
pub type IndexBuffers = mem::DeviceBufferStorage<IndexBuffer>;
pub type AsyncVertexBuffers = mem::AsyncDeviceBufferStorage<VertexBuffer>;
pub type AsyncIndexBuffers = mem::AsyncDeviceBufferStorage<IndexBuffer>;
