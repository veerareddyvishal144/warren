use crate::{Error, InstanceType, Result, Shape, ShapeRequest, Template};

struct Spec {
    cpu: u32,
    memory: u32,
    default_disk: u32,
    max_disk: u32,
}

const CATALOG: [Spec; 4] = [
    Spec {
        cpu: 2,
        memory: 4,
        default_disk: 4,
        max_disk: 12,
    },
    Spec {
        cpu: 4,
        memory: 8,
        default_disk: 6,
        max_disk: 20,
    },
    Spec {
        cpu: 4,
        memory: 16,
        default_disk: 12,
        max_disk: 40,
    },
    Spec {
        cpu: 4,
        memory: 32,
        default_disk: 24,
        max_disk: 80,
    },
];

/// vCPU ceiling on every type.
pub const MAX_CPU: u32 = 4;
/// Smallest disk on every shape.
pub const MIN_DISK_GB: u32 = 2;
/// Largest disk on the `vm` and `performance` types.
pub const MAX_DISK_GB_VM: u32 = 100;
/// Largest disk on the `mac` type. The disk starts at the base image's size
/// and only grows; the runtime enforces the lower bound.
pub const MAX_DISK_GB_MAC: u32 = 500;

/// The CPU/RAM pairs with their default disks.
pub fn valid_shapes() -> Vec<Shape> {
    CATALOG
        .iter()
        .map(|s| Shape {
            cpu: s.cpu,
            memory: s.memory,
            disk: s.default_disk,
        })
        .collect()
}

fn lookup(cpu: u32, memory: u32) -> Option<&'static Spec> {
    CATALOG.iter().find(|s| s.cpu == cpu && s.memory == memory)
}

/// Fill defaults and validate a requested shape. A missing request yields the
/// template's default shape. CPU above `MAX_CPU` is clamped while keeping the
/// requested memory. Disk must be a whole number of GB within the shape's
/// range (up to 100 GB on vm and performance types, 500 GB on mac). On the
/// `mac` type an unrequested disk keeps the template's size whatever the
/// CPU/RAM pair, because the disk is the base image's and never shrinks.
pub fn normalize(
    req: Option<&ShapeRequest>,
    tpl: Option<&Template>,
    t: InstanceType,
) -> Result<Shape> {
    let base = tpl
        .filter(|t| t.default_shape.cpu > 0)
        .map(|t| t.default_shape)
        .unwrap_or(Shape {
            cpu: 2,
            memory: 4,
            disk: 0,
        });
    let mut out = base;
    let mut disk_requested = false;
    if let Some(r) = req {
        if let Some(c) = r.cpu {
            out.cpu = c;
        }
        if let Some(m) = r.memory {
            out.memory = m;
        }
        if let Some(d) = r.disk {
            out.disk = d;
            disk_requested = true;
        }
    }
    if out.cpu == 0 || out.memory == 0 {
        return Err(Error::invalid("cpu and memory must be positive"));
    }
    if out.cpu > MAX_CPU {
        out.cpu = MAX_CPU;
    }
    let spec = lookup(out.cpu, out.memory).ok_or_else(|| {
        Error::invalid(format!(
            "unsupported cpu/memory shape {} vCPU / {} GB; valid shapes are 2/4, 4/8, 4/16, 4/32",
            out.cpu, out.memory
        ))
    })?;
    if !disk_requested && (out.disk == 0 || (out.cpu, out.memory) != (base.cpu, base.memory)) {
        let keep_template_disk = out.disk > 0
            && (t == InstanceType::Mac || (out.cpu, out.memory) == (base.cpu, base.memory));
        out.disk = if keep_template_disk {
            out.disk
        } else {
            spec.default_disk
        };
    }
    let max_disk = max_disk_for(spec, t);
    if out.disk < MIN_DISK_GB || out.disk > max_disk {
        return Err(Error::invalid(format!(
            "disk must be between {MIN_DISK_GB} and {max_disk} GB for this shape"
        )));
    }
    Ok(out)
}

fn max_disk_for(spec: &Spec, t: InstanceType) -> u32 {
    match t {
        InstanceType::Vm | InstanceType::Performance => MAX_DISK_GB_VM,
        InstanceType::Mac => MAX_DISK_GB_MAC,
        InstanceType::Default => spec.max_disk,
    }
}

/// Maximum disk for a shape on the given type.
pub fn max_disk(shape: Shape, t: InstanceType) -> u32 {
    match t {
        InstanceType::Vm | InstanceType::Performance => MAX_DISK_GB_VM,
        InstanceType::Mac => MAX_DISK_GB_MAC,
        InstanceType::Default => lookup(shape.cpu, shape.memory)
            .map(|s| s.max_disk)
            .unwrap_or(12),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tpl() -> Template {
        Template {
            default_shape: Shape {
                cpu: 2,
                memory: 4,
                disk: 4,
            },
            ..Default::default()
        }
    }

    #[test]
    fn defaults_from_template() {
        let s = normalize(None, Some(&tpl()), InstanceType::Default).unwrap();
        assert_eq!(
            s,
            Shape {
                cpu: 2,
                memory: 4,
                disk: 4
            }
        );
    }

    #[test]
    fn clamps_cpu_and_keeps_memory() {
        let r = ShapeRequest {
            cpu: Some(16),
            memory: Some(32),
            disk: None,
        };
        let s = normalize(Some(&r), Some(&tpl()), InstanceType::Default).unwrap();
        assert_eq!(
            s,
            Shape {
                cpu: 4,
                memory: 32,
                disk: 24
            }
        );
    }

    #[test]
    fn rejects_bad_pairs_and_disks() {
        let r = ShapeRequest {
            cpu: Some(2),
            memory: Some(8),
            disk: None,
        };
        assert!(normalize(Some(&r), Some(&tpl()), InstanceType::Default).is_err());
        let r = ShapeRequest {
            cpu: None,
            memory: None,
            disk: Some(50),
        };
        assert!(normalize(Some(&r), Some(&tpl()), InstanceType::Default).is_err());
        assert!(normalize(Some(&r), Some(&tpl()), InstanceType::Vm).is_ok());
    }
}
