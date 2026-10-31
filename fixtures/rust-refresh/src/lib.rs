pub mod inventory;
pub mod order;

use inventory::Stock;

pub trait Deduct {
    fn deduct(&self, qty: u32) -> u32;
}

impl Deduct for Stock {
    fn deduct(&self, qty: u32) -> u32 {
        self.reserve(qty)
    }
}

pub fn dispatch(stock: &dyn Deduct) -> u32 {
    stock.deduct(1)
}
