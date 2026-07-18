use varve::varve_format;

varve_format! {
    pub format MatrixKeyIndexFormat {
        magic: b"KIDM";
        version: 1;
        dims {
            rows: u32,
            columns: u32,
        }
        commit: cell_bitmap {
            keyspace = [row, column];
            categories = [data];
        };
        blocks {
            matrix Cell(
                id = 1,
                dims = [row, column],
                category = data,
                key_index = disk,
            ) {
                value: u64,
            }
        }
    }
}

fn main() {}
