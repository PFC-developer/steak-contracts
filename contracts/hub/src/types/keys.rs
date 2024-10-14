use std::marker::PhantomData;

use cw_storage_plus::{Key, Prefixer, PrimaryKey};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BooleanKey {
    pub wrapped: Vec<u8>,
    pub data: PhantomData<bool>,
}

impl BooleanKey {
    pub fn new(val: bool) -> Self {
        BooleanKey {
            wrapped: if val {
                vec![1]
            } else {
                vec![0]
            },
            data: PhantomData,
        }
    }
}

impl From<bool> for BooleanKey {
    fn from(val: bool) -> Self {
        Self::new(val)
    }
}

impl PrimaryKey<'_> for BooleanKey {
    type Prefix = ();
    type SubPrefix = ();
    type Suffix = ();
    type SuperSuffix = ();

    fn key(&self) -> Vec<Key> {
        self.wrapped.key()
    }
}

impl Prefixer<'_> for BooleanKey {
    fn prefix(&self) -> Vec<Key> {
        self.wrapped.prefix()
    }
}
