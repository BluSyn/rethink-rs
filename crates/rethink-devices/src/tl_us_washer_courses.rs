//! Course codes shared by LG's US top-load washers T1789EFH_F (WT7300CW) and T17A1EFHU_F
//! (WT7305CV / WT7305CW). Captured on T1789EFH_F; T17A1EFHU_F's modelJSON declares the same
//! catalogs. These are protocol identifiers, not the modelJSON's list order or image indexes.

/// Base courses on the dial.
pub const COURSES: &[(u8, &str)] = &[
    (0x00, "Not selected"),
    (0x01, "Normal"),
    (0x02, "Heavy Duty"),
    (0x04, "Delicates"),
    (0x05, "Waterproof"),
    (0x07, "Speed Wash"),
    (0x08, "Bedding"),
    (0x0c, "Tub Clean"),
    (0x11, "Rinse+Spin"),
    (0x12, "Spin Only"),
    (0x19, "Deep Wash"),
    (0x29, "Pre Wash+Normal"),
];

/// Smart Courses, the cycles that can be downloaded to the Downloaded dial position.
pub const SMART_COURSES: &[(u8, &str)] = &[
    (0x00, "Not selected"),
    (0x64, "Small Load"),
    (0x65, "Single Item"),
    (0x67, "Wrinkle Stop/Gentle Wave"),
    (0x68, "Rapid Clean"),
    (0x69, "Sweat Stains"),
    (0x6a, "Swimwear"),
    (0x6b, "Curtains"),
    (0x6d, "Dress Shirts"),
    (0x6e, "Collar Stains"),
    (0x6f, "Pillow Covers"),
    (0x70, "Lingerie"),
    (0x71, "Baby Wear"),
    (0x73, "Children's Wear"),
    (0x74, "Juice & Food Stains"),
    (0x75, "Baby Bibs"),
    (0x76, "Pet Hair and Odor"),
    (0x77, "EconoWash"),
    (0x78, "Collars and Cuffs"),
    (0x79, "Delicate Dresses"),
    (0x7a, "Half Load Wash"),
    (0x7b, "Full Load Wash"),
    (0x83, "Whites"),
];
