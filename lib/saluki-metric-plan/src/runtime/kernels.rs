//! Kernel instances: built off the data path, shared by the generations whose programs use them.

use std::{
    collections::BTreeMap,
    fmt,
    panic::{catch_unwind, AssertUnwindSafe},
};

use saluki_core::execution::{Kernel, PortOutputs};
use tracing::warn;

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
    /// group, if it has one, whose CPU time the build always measures (a build is long and rare).
    ///
    /// # Errors
    ///
    /// Returns an error if the kernel cannot be built from its parameters.
    pub fn build(kernel: &ProgramKernel, context: KernelBuildContext) -> Result<Self, KernelBuildError> {
        let instance = context.accounted_measured(|| kernel.spec.build(&context))?;
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

    /// Returns the context the kernel was built in, which it is rebuilt in after a panic.
    pub fn context(&self) -> &KernelBuildContext {
        &self.context
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
    /// The number of live generations whose programs use the kernel, in a [`KernelSlab`].
    users: usize,
}

impl KernelSlot {
    /// Creates the slot of a prepared kernel with `port_count` output ports.
    pub fn new(prepared: PreparedKernel, port_count: usize) -> Self {
        let PreparedKernel {
            key,
            spec,
            context,
            kernel,
        } = prepared;
        Self {
            key,
            spec,
            context,
            kernel,
            ports: PortOutputs::new(port_count),
            users: 0,
        }
    }

    /// Builds another, empty instance of the slot's kernel from its spec, in its context, with as many ports: a state
    /// owner's instance of its lineage for another clock epoch, which a reset window creates beside this one.
    ///
    /// # Errors
    ///
    /// Returns why the kernel could not be built, because its factory failed or panicked.
    pub fn build_fresh(&self) -> Result<Self, String> {
        let built = catch_unwind(AssertUnwindSafe(|| {
            self.context.accounted(|| self.spec.build(&self.context))
        }));
        let kernel = match built {
            Ok(Ok(kernel)) => kernel,
            Ok(Err(error)) => return Err(error.to_string()),
            Err(payload) => return Err(format!("its factory panicked: {}", panic_message(&*payload))),
        };
        Ok(Self {
            key: self.key.clone(),
            spec: self.spec.clone(),
            context: self.context.clone(),
            kernel,
            ports: PortOutputs::new(self.ports.port_count()),
            users: 0,
        })
    }

    /// Replaces the kernel with a new instance built from its spec, in its context, after a panic left it
    /// inconsistent.
    ///
    /// The instance that panicked is dropped under supervision too, since dropping it can run its state's code.
    ///
    /// # Errors
    ///
    /// Returns why the kernel could not be rebuilt, because its factory failed or panicked; the slot then keeps the
    /// instance that panicked.
    pub fn rebuild(&mut self) -> Result<(), String> {
        let rebuilt = catch_unwind(AssertUnwindSafe(|| {
            self.context.accounted(|| self.spec.build(&self.context))
        }));
        let rebuilt = match rebuilt {
            Ok(Ok(rebuilt)) => rebuilt,
            Ok(Err(error)) => return Err(error.to_string()),
            Err(payload) => return Err(format!("its factory panicked: {}", panic_message(&*payload))),
        };
        let replaced = std::mem::replace(&mut self.kernel, rebuilt);
        let context = &self.context;
        if catch_unwind(AssertUnwindSafe(|| context.accounted(|| drop(replaced)))).is_err() {
            warn!(
                node = %self.key.node,
                "Dropping the instance of a kernel that panicked panicked too; its rebuilt instance runs on."
            );
        }
        Ok(())
    }
}

/// Returns the message a panic carried.
pub(super) fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<panic payload that is not a string>".to_owned())
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
        let mut slot = KernelSlot::new(prepared, port_count);
        slot.users = 1;
        let key = slot.key.clone();
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
