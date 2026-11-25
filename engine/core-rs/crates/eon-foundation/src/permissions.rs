use std::ops::BitOr;

/// A closed set of grants. An empty set grants nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Permissions(u32);

impl Permissions {
    pub const NONE: Self = Self(0);
    pub const READ_WORKSPACE: Self = Self(1 << 0);
    pub const MUTATE_WORKSPACE: Self = Self(1 << 1);
    pub const EXECUTE_PROCESS: Self = Self(1 << 2);
    pub const CYBER_OPERATIONS: Self = Self(1 << 3);
    pub const NETWORK_ACCESS: Self = Self(1 << 4);
    pub const DEVICE_CONTROL: Self = Self(1 << 5);
    pub const SPAWN_SUBAGENT: Self = Self(1 << 6);
    pub const USE_CREDENTIALS: Self = Self(1 << 7);
    pub const MANAGE_CREDENTIALS: Self = Self(1 << 8);

    pub const fn contains(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }
}

impl BitOr for Permissions {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}
