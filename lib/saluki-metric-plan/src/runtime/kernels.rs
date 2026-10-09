//! Kernel instances: built off the data path, shared by the generations whose programs use them.

use std::{collections::BTreeMap, fmt};

use saluki_core::execution::{Kernel, PortOutputs};

use super::RegionError;
use crate::{
    compile::{KernelKey, ProgramKernel, RegionProgram},
    ir::NodeId,
    operators::{KernelBuildContext, KernelBuildError, KernelSpec},
};

/// A kernel instance built for a region, with what the region needs to rebuild it.
///
/// Kernels are built off the data path, where a generation is prepared, and handed to the region with the program
/// that uses them.
pub struct PreparedKernel {
    key: KernelKey,
    spec: KernelSpec,
    context: KernelBuildContext,
    kernel: Box<dyn Kernel>,
}

impl PreparedKernel {
    /// Builds a kernel of a program, in the context the host reports it under, and inside the context's resource
    /// group, if it has one.
    ///
    /// # Errors
    ///
    /// Returns an error if the kernel cannot be built from its parameters.
    pub fn build(kernel: &ProgramKernel, context: KernelBuildContext) -> Result<Self, KernelBuildError> {
        let instance = context.accounted(|| kernel.spec.build(&context))?;
        Ok(Self {
            key: kernel.key.clone(),
            spec: kernel.spec.clone(),
            context,
            kernel: instance,
        })
    }

    /// Returns the kernel's key.
    pub fn key(&self) -> &KernelKey {
        &self.key
    }
}

impl fmt::Debug for PreparedKernel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedKernel")
            .field("key", &self.key)
            .field("context", &self.context)
            .finish_non_exhaustive()
    }
}

/// Builds the kernels of a program with the given keys, each in the context `contexts` returns for its node.
///
/// A region's creator builds every kernel of the boot program this way, and the installer the kernels a preparation
/// lists to build.
///
/// # Errors
///
/// Returns an error if the program has no kernel with one of the keys, or if a kernel cannot be built.
pub fn build_kernels<'k, K, C>(
    program: &RegionProgram, keys: K, contexts: C,
) -> Result<Vec<PreparedKernel>, RegionError>
where
    K: IntoIterator<Item = &'k KernelKey>,
    C: Fn(&NodeId) -> KernelBuildContext,
{
    keys.into_iter()
        .map(|key| {
            let kernel = program.kernel_by_key(key).ok_or_else(|| RegionError::UnknownKernel {
                slot: program.slot(),
                key: key.clone(),
            })?;
            PreparedKernel::build(kernel, contexts(&key.node)).map_err(|source| RegionError::KernelBuild {
                node: key.node.clone(),
                source,
            })
        })
        .collect()
}

/// One kernel instance of a region.
pub(super) struct KernelSlot {
    pub key: KernelKey,
    pub spec: KernelSpec,
    pub context: KernelBuildContext,
    pub kernel: Box<dyn Kernel>,
    /// The kernel's output ports, reused by every call.
    pub ports: PortOutputs,
    /// The number of live generations whose programs use the kernel.
    users: usize,
}

/// The kernel instances of a region, keyed by [`KernelKey`] and shared by every generation whose program has the key.
///
/// Each instance counts the live generations that use it, and is dropped when the last one is released.
#[derive(Default)]
pub(super) struct KernelSlab {
    slots: Vec<Option<KernelSlot>>,
    free: Vec<usize>,
    index: BTreeMap<KernelKey, usize>,
}

impl KernelSlab {
    /// Returns the instance with the given key, if the region has one.
    pub fn find(&self, key: &KernelKey) -> Option<usize> {
        self.index.get(key).copied()
    }

    /// Adds a use of the instance at `index`.
    pub fn acquire(&mut self, index: usize) {
        if let Some(slot) = self.slots.get_mut(index).and_then(Option::as_mut) {
            slot.users += 1;
        }
    }

    /// Adds a new instance with one use, returning its index.
    pub fn insert(&mut self, prepared: PreparedKernel, port_count: usize) -> usize {
        let PreparedKernel {
            key,
            spec,
            context,
            kernel,
        } = prepared;
        let slot = KernelSlot {
            key: key.clone(),
            spec,
            context,
            kernel,
            ports: PortOutputs::new(port_count),
            users: 1,
        };
        let index = match self.free.pop() {
            Some(index) => {
                self.slots[index] = Some(slot);
                index
            }
            None => {
                self.slots.push(Some(slot));
                self.slots.len() - 1
            }
        };
        self.index.insert(key, index);
        index
    }

    /// Removes a use of the instance at `index`, dropping the instance when it was the last.
    pub fn release(&mut self, index: usize) {
        let Some(entry) = self.slots.get_mut(index) else { return };
        let Some(slot) = entry.as_mut() else { return };
        slot.users = slot.users.saturating_sub(1);
        if slot.users == 0 {
            if let Some(slot) = entry.take() {
                self.index.remove(&slot.key);
            }
            self.free.push(index);
        }
    }

    /// Returns the instance at `index`.
    ///
    /// # Panics
    ///
    /// Panics if there is no instance at `index`: every program's kernel indices name instances it holds a use of.
    pub fn get_mut(&mut self, index: usize) -> &mut KernelSlot {
        self.slots
            .get_mut(index)
            .and_then(Option::as_mut)
            .expect("a live generation's kernels are in the slab")
    }

    /// Returns every instance.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut KernelSlot> {
        self.slots.iter_mut().flatten()
    }

    /// Returns the number of instances.
    pub fn len(&self) -> usize {
        self.index.len()
    }
}
