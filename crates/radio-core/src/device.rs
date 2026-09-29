use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SdrDevice {
    pub index: usize,
    pub bus: String,
    pub address: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub serial: Option<String>,
}

pub struct DeviceInventory;

impl DeviceInventory {
    #[cfg(feature = "hardware")]
    pub fn discover() -> Result<Vec<SdrDevice>, rs_rtl::Error> {
        let descriptors = rs_rtl::DeviceDescriptors::new()?;
        Ok(descriptors
            .iter()
            .map(|device| SdrDevice {
                index: device.index,
                bus: device.bus.clone(),
                address: device.address,
                vendor_id: device.vendor_id,
                product_id: device.product_id,
                manufacturer: device.manufacturer.clone(),
                product: device.product.clone(),
                serial: device.serial.clone(),
            })
            .collect())
    }

    #[cfg(not(feature = "hardware"))]
    pub fn discover() -> Result<Vec<SdrDevice>, std::convert::Infallible> {
        Ok(Vec::new())
    }
}
