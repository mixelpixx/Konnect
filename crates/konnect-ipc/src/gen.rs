//! Auto-generated protobuf types from KiCAD 10 API .proto files.
//! These are compiled by prost-build in build.rs.

pub mod kiapi {
    pub mod common {
        include!(concat!(env!("OUT_DIR"), "/kiapi.common.rs"));

        pub mod types {
            include!(concat!(env!("OUT_DIR"), "/kiapi.common.types.rs"));
        }

        pub mod commands {
            include!(concat!(env!("OUT_DIR"), "/kiapi.common.commands.rs"));
        }

        pub mod project {
            include!(concat!(env!("OUT_DIR"), "/kiapi.common.project.rs"));
        }
    }

    pub mod board {
        include!(concat!(env!("OUT_DIR"), "/kiapi.board.rs"));

        pub mod types {
            include!(concat!(env!("OUT_DIR"), "/kiapi.board.types.rs"));
        }

        pub mod commands {
            include!(concat!(env!("OUT_DIR"), "/kiapi.board.commands.rs"));
        }
    }

    pub mod schematic {
        pub mod types {
            include!(concat!(env!("OUT_DIR"), "/kiapi.schematic.types.rs"));
        }
    }
}
