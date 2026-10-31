use crate::inventory::{Stock, available};

pub fn create(stock: &Stock) -> u32 {
    let _before = available();
    stock.reserve(1)
}
