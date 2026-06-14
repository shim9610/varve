use varve::varve_format;

varve_format! {
    pub format BadMatrixFormat {
        magic: b"BMTX";
        version: 1;
        dims {
            scan: u32,
            ch: u32,
        }
        commit: cell_bitmap {
            keyspace = [scan, ch];
            categories = [analysis];
        };
        blocks {
            matrix BadCell(id = 1, dims = [scan, ch], category = analysis) {
                value: String,
            }
        }
    }
}

fn main() {}
