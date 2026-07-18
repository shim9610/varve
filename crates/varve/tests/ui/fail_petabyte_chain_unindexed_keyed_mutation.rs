use varve::varve_format;

varve_format! {
    pub format PetabyteChainIndexedFormat {
        magic: b"PTCI";
        version: 1;
        index: keyed_offset_chain;
        blocks {
            fixed Account(id = 1, key = [id], key_index = memory) {
                id: u64,
                active: bool,
            }

            variable Frame(id = 2, key = [id], key_index = disk) {
                id: u64,
                payload: Vec<u8>,
            }
        }
    }
}

fn unindexed_keyed_mutation_is_not_exposed(
    writer: &mut PetabyteChainIndexedFormatIndexedWriter,
    account: &Account,
) {
    let _ = writer.push_account(account);
}

fn main() {}
