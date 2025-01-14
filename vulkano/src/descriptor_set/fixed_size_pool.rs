// Copyright (c) 2017 The vulkano developers
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or https://opensource.org/licenses/MIT>,
// at your option. All files in the project carrying such
// notice may not be copied, modified, or distributed except
// according to those terms.

//! Pool of descriptor sets of a specific capacity that are automatically reclaimed.
//!
//! You are encouraged to use this type when you need a different descriptor set at each frame, or
//! regularly during the execution.
//!
//! # Example
//!
//! At initialization, create a `FixedSizeDescriptorSetsPool`.
//!
//! ```rust
//! use vulkano::descriptor_set::FixedSizeDescriptorSetsPool;
//! # use vulkano::pipeline::GraphicsPipeline;
//! # use std::sync::Arc;
//! # let graphics_pipeline: Arc<GraphicsPipeline> = return;
//! // use vulkano::pipeline::GraphicsPipeline;
//! // let graphics_pipeline: Arc<GraphicsPipeline> = ...;
//!
//! let layout = graphics_pipeline.layout().descriptor_set_layouts().get(0).unwrap();
//! let pool = FixedSizeDescriptorSetsPool::new(layout.clone());
//! ```
//!
//! You would then typically store the pool in a struct for later. Then whenever you need a
//! descriptor set, call `pool.next()` to start the process of building it.
//!
//! ```rust
//! # use std::sync::Arc;
//! # use vulkano::descriptor_set::FixedSizeDescriptorSetsPool;
//! # use vulkano::pipeline::GraphicsPipeline;
//! # let mut pool: FixedSizeDescriptorSetsPool = return;
//! let descriptor_set = pool.next()
//!     //.add_buffer(...)
//!     //.add_sampled_image(...)
//!     .build().unwrap();
//! ```
//!
//! Note that `next()` requires exclusive (`mut`) access to the pool. You can use a `Mutex` around
//! the pool if you can't provide this.

use crate::buffer::BufferAccess;
use crate::buffer::BufferViewRef;
use crate::descriptor_set::layout::DescriptorSetLayout;
use crate::descriptor_set::persistent::*;
use crate::descriptor_set::pool::DescriptorPool;
use crate::descriptor_set::pool::DescriptorPoolAlloc;
use crate::descriptor_set::pool::DescriptorPoolAllocError;
use crate::descriptor_set::pool::UnsafeDescriptorPool;
use crate::descriptor_set::DescriptorSet;
use crate::descriptor_set::UnsafeDescriptorSet;
use crate::device::Device;
use crate::device::DeviceOwned;
use crate::image::view::ImageViewAbstract;
use crate::sampler::Sampler;
use crate::OomError;
use crate::VulkanObject;
use crossbeam_queue::SegQueue;
use std::hash::Hash;
use std::hash::Hasher;
use std::sync::Arc;

/// Pool of descriptor sets of a specific capacity that are automatically reclaimed.
#[derive(Clone)]
pub struct FixedSizeDescriptorSetsPool {
    layout: Arc<DescriptorSetLayout>,
    // We hold a local implementation of the `DescriptorPool` trait for our own purpose. Since we
    // don't want to expose this trait impl in our API, we use a separate struct.
    pool: LocalPool,
}

impl FixedSizeDescriptorSetsPool {
    /// Initializes a new pool. The pool is configured to allocate sets that corresponds to the
    /// parameters passed to this function.
    pub fn new(layout: Arc<DescriptorSetLayout>) -> FixedSizeDescriptorSetsPool {
        let device = layout.device().clone();

        FixedSizeDescriptorSetsPool {
            layout,
            pool: LocalPool {
                device,
                next_capacity: 3,
                current_pool: None,
            },
        }
    }

    /// Starts the process of building a new descriptor set.
    ///
    /// The set will corresponds to the set layout that was passed to `new`.
    #[inline]
    pub fn next(&mut self) -> FixedSizeDescriptorSetBuilder<()> {
        let inner = PersistentDescriptorSet::start(self.layout.clone());

        FixedSizeDescriptorSetBuilder { pool: self, inner }
    }
}

/// A descriptor set created from a `FixedSizeDescriptorSetsPool`.
pub struct FixedSizeDescriptorSet<R> {
    inner: PersistentDescriptorSet<R, LocalPoolAlloc>,
}

unsafe impl<R> DescriptorSet for FixedSizeDescriptorSet<R>
where
    R: PersistentDescriptorSetResources,
{
    #[inline]
    fn inner(&self) -> &UnsafeDescriptorSet {
        self.inner.inner()
    }

    #[inline]
    fn layout(&self) -> &Arc<DescriptorSetLayout> {
        self.inner.layout()
    }

    #[inline]
    fn num_buffers(&self) -> usize {
        self.inner.num_buffers()
    }

    #[inline]
    fn buffer(&self, index: usize) -> Option<(&dyn BufferAccess, u32)> {
        self.inner.buffer(index)
    }

    #[inline]
    fn num_images(&self) -> usize {
        self.inner.num_images()
    }

    #[inline]
    fn image(&self, index: usize) -> Option<(&dyn ImageViewAbstract, u32)> {
        self.inner.image(index)
    }
}

unsafe impl<R> DeviceOwned for FixedSizeDescriptorSet<R> {
    #[inline]
    fn device(&self) -> &Arc<Device> {
        self.inner.device()
    }
}

impl<R> PartialEq for FixedSizeDescriptorSet<R>
where
    R: PersistentDescriptorSetResources,
{
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.inner().internal_object() == other.inner().internal_object()
            && self.device() == other.device()
    }
}

impl<R> Eq for FixedSizeDescriptorSet<R> where R: PersistentDescriptorSetResources {}

impl<R> Hash for FixedSizeDescriptorSet<R>
where
    R: PersistentDescriptorSetResources,
{
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.inner().internal_object().hash(state);
        self.device().hash(state);
    }
}

// The fields of this struct can be considered as fields of the `FixedSizeDescriptorSet`. They are
// in a separate struct because we don't want to expose the fact that we implement the
// `DescriptorPool` trait.
#[derive(Clone)]
struct LocalPool {
    // The `LocalPoolInner` struct contains an actual Vulkan pool. Every time it is full, we create
    // a new pool and replace the current one with the new one.
    current_pool: Option<Arc<LocalPoolInner>>,
    // Capacity to use when we create a new Vulkan pool.
    next_capacity: u32,
    // The Vulkan device.
    device: Arc<Device>,
}

struct LocalPoolInner {
    // The actual Vulkan descriptor pool. This field isn't actually used anywhere, but we need to
    // keep the pool alive in order to keep the descriptor sets valid.
    actual_pool: UnsafeDescriptorPool,

    // List of descriptor sets. When `alloc` is called, a descriptor will be extracted from this
    // list. When a `LocalPoolAlloc` is dropped, its descriptor set is put back in this list.
    reserve: SegQueue<UnsafeDescriptorSet>,
}

struct LocalPoolAlloc {
    // The `LocalPoolInner` we were allocated from. We need to keep a copy of it in each allocation
    // so that we can put back the allocation in the list in our `Drop` impl.
    pool: Arc<LocalPoolInner>,

    // The actual descriptor set, wrapped inside an `Option` so that we can extract it in our
    // `Drop` impl.
    actual_alloc: Option<UnsafeDescriptorSet>,
}

unsafe impl DescriptorPool for LocalPool {
    type Alloc = LocalPoolAlloc;

    fn alloc(&mut self, layout: &DescriptorSetLayout) -> Result<Self::Alloc, OomError> {
        loop {
            // Try to extract a descriptor from the current pool if any exist.
            // This is the most common case.
            if let Some(ref mut current_pool) = self.current_pool {
                if let Some(already_existing_set) = current_pool.reserve.pop() {
                    return Ok(LocalPoolAlloc {
                        actual_alloc: Some(already_existing_set),
                        pool: current_pool.clone(),
                    });
                }
            }

            // If we failed to grab an existing set, that means the current pool is full. Create a
            // new one of larger capacity.
            let count = *layout.descriptors_count() * self.next_capacity;
            let mut new_pool =
                UnsafeDescriptorPool::new(self.device.clone(), &count, self.next_capacity, false)?;
            let alloc = unsafe {
                match new_pool.alloc((0..self.next_capacity).map(|_| layout)) {
                    Ok(iter) => {
                        let stack = SegQueue::new();
                        for elem in iter {
                            stack.push(elem);
                        }
                        stack
                    }
                    Err(DescriptorPoolAllocError::OutOfHostMemory) => {
                        return Err(OomError::OutOfHostMemory);
                    }
                    Err(DescriptorPoolAllocError::OutOfDeviceMemory) => {
                        return Err(OomError::OutOfDeviceMemory);
                    }
                    Err(DescriptorPoolAllocError::FragmentedPool) => {
                        // This can't happen as we don't free individual sets.
                        unreachable!()
                    }
                    Err(DescriptorPoolAllocError::OutOfPoolMemory) => unreachable!(),
                }
            };

            self.next_capacity = self.next_capacity.saturating_mul(2);
            self.current_pool = Some(Arc::new(LocalPoolInner {
                actual_pool: new_pool,
                reserve: alloc,
            }));
        }
    }
}

unsafe impl DeviceOwned for LocalPool {
    #[inline]
    fn device(&self) -> &Arc<Device> {
        &self.device
    }
}

impl DescriptorPoolAlloc for LocalPoolAlloc {
    #[inline]
    fn inner(&self) -> &UnsafeDescriptorSet {
        self.actual_alloc.as_ref().unwrap()
    }

    #[inline]
    fn inner_mut(&mut self) -> &mut UnsafeDescriptorSet {
        self.actual_alloc.as_mut().unwrap()
    }
}

impl Drop for LocalPoolAlloc {
    fn drop(&mut self) {
        let inner = self.actual_alloc.take().unwrap();
        self.pool.reserve.push(inner);
    }
}

/// Prototype of a `FixedSizeDescriptorSet`.
///
/// The template parameter `R` is an unspecified type that represents the list of resources.
///
/// See the docs of `FixedSizeDescriptorSetsPool` for an example.
pub struct FixedSizeDescriptorSetBuilder<'a, R> {
    pool: &'a mut FixedSizeDescriptorSetsPool,
    inner: PersistentDescriptorSetBuilder<R>,
}

impl<'a, R> FixedSizeDescriptorSetBuilder<'a, R> {
    /// Builds a `FixedSizeDescriptorSet` from the builder.
    #[inline]
    pub fn build(self) -> Result<FixedSizeDescriptorSet<R>, PersistentDescriptorSetBuildError> {
        let inner = self.inner.build_with_pool(&mut self.pool.pool)?;
        Ok(FixedSizeDescriptorSet { inner })
    }

    /// Call this function if the next element of the set is an array in order to set the value of
    /// each element.
    ///
    /// Returns an error if the descriptor is empty.
    ///
    /// This function can be called even if the descriptor isn't an array, and it is valid to enter
    /// the "array", add one element, then leave.
    #[inline]
    pub fn enter_array(
        self,
    ) -> Result<FixedSizeDescriptorSetBuilderArray<'a, R>, PersistentDescriptorSetError> {
        Ok(FixedSizeDescriptorSetBuilderArray {
            pool: self.pool,
            inner: self.inner.enter_array()?,
        })
    }

    /// Skips the current descriptor if it is empty.
    #[inline]
    pub fn add_empty(
        self,
    ) -> Result<FixedSizeDescriptorSetBuilder<'a, R>, PersistentDescriptorSetError> {
        Ok(FixedSizeDescriptorSetBuilder {
            pool: self.pool,
            inner: self.inner.add_empty()?,
        })
    }

    /// Binds a buffer as the next descriptor.
    ///
    /// An error is returned if the buffer isn't compatible with the descriptor.
    ///
    /// # Panic
    ///
    /// Panics if the buffer doesn't have the same device as the descriptor set layout.
    ///
    #[inline]
    pub fn add_buffer<T>(
        self,
        buffer: T,
    ) -> Result<
        FixedSizeDescriptorSetBuilder<'a, (R, PersistentDescriptorSetBuf<T>)>,
        PersistentDescriptorSetError,
    >
    where
        T: BufferAccess,
    {
        Ok(FixedSizeDescriptorSetBuilder {
            pool: self.pool,
            inner: self.inner.add_buffer(buffer)?,
        })
    }

    /// Binds a buffer view as the next descriptor.
    ///
    /// An error is returned if the buffer isn't compatible with the descriptor.
    ///
    /// # Panic
    ///
    /// Panics if the buffer view doesn't have the same device as the descriptor set layout.
    ///
    pub fn add_buffer_view<T>(
        self,
        view: T,
    ) -> Result<
        FixedSizeDescriptorSetBuilder<'a, (R, PersistentDescriptorSetBufView<T>)>,
        PersistentDescriptorSetError,
    >
    where
        T: BufferViewRef,
    {
        Ok(FixedSizeDescriptorSetBuilder {
            pool: self.pool,
            inner: self.inner.add_buffer_view(view)?,
        })
    }

    /// Binds an image view as the next descriptor.
    ///
    /// An error is returned if the image view isn't compatible with the descriptor.
    ///
    /// # Panic
    ///
    /// Panics if the image view doesn't have the same device as the descriptor set layout.
    ///
    #[inline]
    pub fn add_image<T>(
        self,
        image_view: T,
    ) -> Result<
        FixedSizeDescriptorSetBuilder<'a, (R, PersistentDescriptorSetImg<T>)>,
        PersistentDescriptorSetError,
    >
    where
        T: ImageViewAbstract,
    {
        Ok(FixedSizeDescriptorSetBuilder {
            pool: self.pool,
            inner: self.inner.add_image(image_view)?,
        })
    }

    /// Binds an image view with a sampler as the next descriptor.
    ///
    /// An error is returned if the image view isn't compatible with the descriptor.
    ///
    /// # Panic
    ///
    /// Panics if the image view or the sampler doesn't have the same device as the descriptor set layout.
    ///
    #[inline]
    pub fn add_sampled_image<T>(
        self,
        image_view: T,
        sampler: Arc<Sampler>,
    ) -> Result<
        FixedSizeDescriptorSetBuilder<
            'a,
            (
                (R, PersistentDescriptorSetImg<T>),
                PersistentDescriptorSetSampler,
            ),
        >,
        PersistentDescriptorSetError,
    >
    where
        T: ImageViewAbstract,
    {
        Ok(FixedSizeDescriptorSetBuilder {
            pool: self.pool,
            inner: self.inner.add_sampled_image(image_view, sampler)?,
        })
    }

    /// Binds a sampler as the next descriptor.
    ///
    /// An error is returned if the sampler isn't compatible with the descriptor.
    ///
    /// # Panic
    ///
    /// Panics if the sampler doesn't have the same device as the descriptor set layout.
    ///
    #[inline]
    pub fn add_sampler(
        self,
        sampler: Arc<Sampler>,
    ) -> Result<
        FixedSizeDescriptorSetBuilder<'a, (R, PersistentDescriptorSetSampler)>,
        PersistentDescriptorSetError,
    > {
        Ok(FixedSizeDescriptorSetBuilder {
            pool: self.pool,
            inner: self.inner.add_sampler(sampler)?,
        })
    }
}

/// Same as `FixedSizeDescriptorSetBuilder`, but we're in an array.
pub struct FixedSizeDescriptorSetBuilderArray<'a, R> {
    pool: &'a mut FixedSizeDescriptorSetsPool,
    inner: PersistentDescriptorSetBuilderArray<R>,
}

impl<'a, R> FixedSizeDescriptorSetBuilderArray<'a, R> {
    /// Leaves the array. Call this once you added all the elements of the array.
    pub fn leave_array(
        self,
    ) -> Result<FixedSizeDescriptorSetBuilder<'a, R>, PersistentDescriptorSetError> {
        Ok(FixedSizeDescriptorSetBuilder {
            pool: self.pool,
            inner: self.inner.leave_array()?,
        })
    }

    /// Binds a buffer as the next element in the array.
    ///
    /// An error is returned if the buffer isn't compatible with the descriptor.
    ///
    /// # Panic
    ///
    /// Panics if the buffer doesn't have the same device as the descriptor set layout.
    ///
    pub fn add_buffer<T>(
        self,
        buffer: T,
    ) -> Result<
        FixedSizeDescriptorSetBuilderArray<'a, (R, PersistentDescriptorSetBuf<T>)>,
        PersistentDescriptorSetError,
    >
    where
        T: BufferAccess,
    {
        Ok(FixedSizeDescriptorSetBuilderArray {
            pool: self.pool,
            inner: self.inner.add_buffer(buffer)?,
        })
    }

    /// Binds a buffer view as the next element in the array.
    ///
    /// An error is returned if the buffer isn't compatible with the descriptor.
    ///
    /// # Panic
    ///
    /// Panics if the buffer view doesn't have the same device as the descriptor set layout.
    ///
    pub fn add_buffer_view<T>(
        self,
        view: T,
    ) -> Result<
        FixedSizeDescriptorSetBuilderArray<'a, (R, PersistentDescriptorSetBufView<T>)>,
        PersistentDescriptorSetError,
    >
    where
        T: BufferViewRef,
    {
        Ok(FixedSizeDescriptorSetBuilderArray {
            pool: self.pool,
            inner: self.inner.add_buffer_view(view)?,
        })
    }

    /// Binds an image view as the next element in the array.
    ///
    /// An error is returned if the image view isn't compatible with the descriptor.
    ///
    /// # Panic
    ///
    /// Panics if the image view doesn't have the same device as the descriptor set layout.
    ///
    pub fn add_image<T>(
        self,
        image_view: T,
    ) -> Result<
        FixedSizeDescriptorSetBuilderArray<'a, (R, PersistentDescriptorSetImg<T>)>,
        PersistentDescriptorSetError,
    >
    where
        T: ImageViewAbstract,
    {
        Ok(FixedSizeDescriptorSetBuilderArray {
            pool: self.pool,
            inner: self.inner.add_image(image_view)?,
        })
    }

    /// Binds an image view with a sampler as the next element in the array.
    ///
    /// An error is returned if the image view isn't compatible with the descriptor.
    ///
    /// # Panic
    ///
    /// Panics if the image or the sampler doesn't have the same device as the descriptor set layout.
    ///
    pub fn add_sampled_image<T>(
        self,
        image_view: T,
        sampler: Arc<Sampler>,
    ) -> Result<
        FixedSizeDescriptorSetBuilderArray<
            'a,
            (
                (R, PersistentDescriptorSetImg<T>),
                PersistentDescriptorSetSampler,
            ),
        >,
        PersistentDescriptorSetError,
    >
    where
        T: ImageViewAbstract,
    {
        Ok(FixedSizeDescriptorSetBuilderArray {
            pool: self.pool,
            inner: self.inner.add_sampled_image(image_view, sampler)?,
        })
    }

    /// Binds a sampler as the next element in the array.
    ///
    /// An error is returned if the sampler isn't compatible with the descriptor.
    ///
    /// # Panic
    ///
    /// Panics if the sampler doesn't have the same device as the descriptor set layout.
    ///
    pub fn add_sampler(
        self,
        sampler: Arc<Sampler>,
    ) -> Result<
        FixedSizeDescriptorSetBuilderArray<'a, (R, PersistentDescriptorSetSampler)>,
        PersistentDescriptorSetError,
    > {
        Ok(FixedSizeDescriptorSetBuilderArray {
            pool: self.pool,
            inner: self.inner.add_sampler(sampler)?,
        })
    }
}
