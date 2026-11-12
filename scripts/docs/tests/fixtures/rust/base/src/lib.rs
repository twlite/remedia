//! Fixture base crate.

mod private;

pub mod config {
    /// Configuration in the `config` module.
    pub struct Settings {
        /// Number of retries.
        pub retries: u32,
    }
}

pub mod other {
    /// A second `Settings` with the same name in another module.
    pub struct Settings;
}

pub use private::Hidden;

/// A source of values.
///
/// # Examples
///
/// ```
/// # use fx_base::Source;
/// # fn demo(s: impl Source) {
/// let item = s.next_item();
/// # }
/// ```
pub trait Source {
    /// The produced item type.
    type Item;

    /// Maximum number of items.
    const LIMIT: usize = 8;

    /// Produce the next item. See [`Source::Item`] and [`Counter`].
    ///
    /// # Errors
    ///
    /// Never fails.
    fn next_item(&mut self) -> Option<Self::Item>;

    /// Provided helper.
    fn peek(&self) -> bool {
        false
    }
}

/// Counts upwards.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Counter {
    /// Current value.
    pub value: u64,
    hidden: u8,
}

impl Counter {
    /// Create a counter at zero.
    pub fn new() -> Self {
        Self { value: 0, hidden: 0 }
    }
}

impl Source for Counter {
    type Item = u64;

    fn next_item(&mut self) -> Option<u64> {
        None
    }
}

/// A function whose name differs only by case from a type.
#[allow(non_snake_case)]
pub fn counter() {}

/// A type whose name differs only by case from a function.
pub struct COUNTER;

/// Shape of a value.
pub enum Shape {
    /// Nothing.
    Empty,
    /// A circle.
    Circle(f64),
    /// A rectangle.
    Rect {
        /// Width.
        width: f64,
        /// Height.
        height: f64,
    },
}

/// Struct and function sharing a name in the same module.
pub struct Twin {
    /// Payload.
    pub x: u8,
}

/// Function twin.
#[allow(non_snake_case)]
pub fn Twin() {}
