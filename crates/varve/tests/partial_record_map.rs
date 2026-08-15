//! `RecordMap`: an index the caller owns, walked only as far as the question
//! needed.
//!
//! The measurement these exist for. On a 20,000-record file, finding the one
//! record at position 12:
//!
//! | route | records framed |
//! | --- | --- |
//! | `index_entries_into` then search | 20,000 (at open) + 20,000 |
//! | `RecordMap::find` | **13** |
//!
//! The correctness these exist for is the other half, and it is the one that
//! could quietly be wrong: a prefix is only a useful answer if it is the *same*
//! prefix a full open would have produced. `a_completed_map_is_the_open_scans_index`
//! is that check, and it is the reason the walk skips non-resident blocks and
//! stops at the snapshot end rather than the file length.

use varve::{RecordDirectory, VarveBlock, VarveFile, varve_format};
// Only the frame-counted tests below name this type; `records_framed` is behind
// the same feature, so the import has to move with them.
#[cfg(feature = "scalable-fault-injection")]
use varve::RecordIndexEntry;

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 1, version = 1, kind = "variable", key = "id")]
struct Note {
    #[varve(field_id = 1)]
    id: u64,
    #[varve(field_id = 2)]
    body: String,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 2, version = 1, kind = "fixed")]
struct Tick {
    at: u64,
}

varve_format! {
    pub struct MapFormat {
        magic: b"RECMAP00";
        version: 1;
        endian: little;
        blocks: [Note, Tick];
    }
}

/// `Note`s are rare and late; `Tick`s are the bulk. A question about the third
/// `Note` is then a question a prefix can answer and a full index would have
/// over-answered.
fn build(path: &std::path::Path, ticks: u64) -> varve::Result<()> {
    let mut writer = MapFormat::create(path)?;
    for at in 0..ticks {
        writer.push(&Tick { at })?;
        if at % 6 == 5 {
            writer.push_keyed(&Note {
                id: at,
                body: format!("note {at}"),
            })?;
        }
    }
    writer.flush()?;
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
fn framed<T>(body: impl FnOnce() -> T) -> (T, u64) {
    let before = VarveFile::records_framed();
    let value = body();
    (value, VarveFile::records_framed() - before)
}

#[test]
fn a_completed_map_is_the_open_scans_index() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("equal.varve");
    build(&path, 400)?;
    let file = MapFormat::open_readonly(&path)?;

    let mut buffer = Vec::new();
    let mut map = file.record_map(&mut buffer)?;
    let count = map.fill()?;
    assert!(map.is_complete());

    // Entry for entry, not merely record for record: the walk must reproduce
    // the committed flag, the footer chain and the payload extents the scan
    // produced, or a read answered through the map is a different read.
    assert_eq!(map.entries(), &*file.index_entries());
    assert_eq!(count, file.index_entries().len());
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
#[test]
fn find_stops_at_the_answer_instead_of_at_the_end() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("stops.varve");
    build(&path, 20_000)?;
    let file = MapFormat::open_readonly(&path)?;
    let total = file.index_entries().len();
    assert!(total > 20_000);

    // The whole index, for comparison: every record framed.
    let (_all, whole) = framed(|| {
        let mut everything = Vec::new();
        file.index_entries_into(&mut everything).expect("into");
        everything.len()
    });
    assert_eq!(
        whole, 0,
        "`index_entries_into` faults an existing directory rather than framing \
         a new index; the framing counter is the *build* unit"
    );

    // The map, asked for the first `Note`. It is the 7th record.
    let mut buffer = Vec::new();
    let mut map = file.record_map(&mut buffer)?;
    let (hit, walked) = framed(|| map.find(|entry| entry.block_id == Note::ID));
    let hit = hit?.expect("a note");
    assert_eq!(hit.block_id, Note::ID);
    assert_eq!(
        walked, 7,
        "the first note is the 7th record; the walk must stop there"
    );
    assert_eq!(map.len(), 7);
    assert!(!map.is_complete());
    assert!(
        walked * 100 < total as u64,
        "the point is the ratio: {walked} framed of {total} records"
    );
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
#[test]
fn a_second_question_resumes_and_a_repeat_reads_nothing() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("resume.varve");
    build(&path, 2_000)?;
    let file = MapFormat::open_readonly(&path)?;

    let mut buffer = Vec::new();
    let mut map = file.record_map(&mut buffer)?;
    let (first, walked_first) = framed(|| map.find(|entry| entry.block_id == Note::ID));
    let first = first?.expect("a note");

    // The next note is six records further on, so the second question costs
    // those six and not another walk from the start.
    let (second, walked_second) = framed(|| {
        map.find(|entry| entry.block_id == Note::ID && entry.record_offset > first.record_offset)
    });
    let second = second?.expect("a second note");
    assert!(second.record_offset > first.record_offset);
    assert_eq!(walked_first, 7);
    assert_eq!(
        walked_second, 7,
        "resumed: the records between the two answers, not the file"
    );

    // Asking again for something already framed reads nothing at all.
    let (again, walked_again) = framed(|| map.find(|entry| entry.block_id == Note::ID));
    assert_eq!(again?.expect("a note"), first);
    assert_eq!(walked_again, 0, "a question the prefix already answers");
    Ok(())
}

#[test]
fn the_map_is_a_directory_every_read_already_understands() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("directory.varve");
    build(&path, 60)?;
    // Opened with no directory of its own: the handle keeps nothing and the
    // map is the only index in the process.
    let mut scan = Vec::new();
    let file = VarveFile::open_readonly_without_directory(MapFormat::spec(), &path, &mut scan)?;
    scan.clear();
    scan.shrink_to_fit();

    let mut buffer = Vec::new();
    let mut map = file.record_map(&mut buffer)?;
    map.fill_to(20)?;
    assert_eq!(map.len(), 20);
    assert_eq!(map.record_count(), 20);
    assert_eq!(map.record_at(3)?, map.entries()[3]);
    assert!(matches!(
        map.record_at(20),
        Err(varve::Error::UnexpectedEof)
    ));

    // The reads the handle itself must refuse are answered against the prefix.
    assert!(matches!(
        file.blocks::<Tick>(),
        Err(varve::Error::NoResidentDirectory { .. })
    ));
    let mut ticks = Vec::new();
    file.with_directory(&map)?
        .block_entries_into::<Tick>(&mut ticks)?;
    assert_eq!(
        ticks.len(),
        map.entries()
            .iter()
            .filter(|entry| entry.block_id == Tick::ID)
            .count()
    );
    assert!(!ticks.is_empty());

    // And a payload read goes through the handle while the map is alive, which
    // is only possible because the map borrows the buffer and not the file.
    let mut payload = Vec::new();
    file.read_payload_into(&ticks[0], &mut payload)?;
    assert_eq!(payload.len() as u64, ticks[0].payload_len);
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
#[test]
fn clear_rewinds_and_release_keeps_the_ground_covered() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("clear.varve");
    build(&path, 300)?;
    let file = MapFormat::open_readonly(&path)?;
    let total = file.index_entries().len();

    let mut buffer = Vec::new();
    let mut map = file.record_map(&mut buffer)?;
    map.fill_to(10)?;
    let resumed_at = map.resume_offset();

    // `release` frees the entries and keeps the position: the next fill starts
    // at record 11, not at record 1.
    map.release();
    assert_eq!(map.len(), 0);
    assert_eq!(map.resume_offset(), resumed_at);
    let (_n, walked) = framed(|| map.fill_to(5));
    assert_eq!(walked, 5, "five more records, not the first five again");

    // `clear` rewinds: the map is what `record_map` just returned.
    map.clear();
    assert_eq!(map.len(), 0);
    assert!(!map.is_complete());
    let (_n, walked) = framed(|| map.fill_to(3));
    assert_eq!(walked, 3);
    assert_eq!(map.entries(), &file.index_entries()[..3]);

    // The bounded-memory pass: every record seen, `k` entries live at a time,
    // and no record read twice.
    map.clear();
    let mut seen = 0usize;
    let (_r, walked) = framed(|| -> varve::Result<()> {
        loop {
            map.fill_to(16)?;
            seen += map.len();
            if map.is_complete() {
                break;
            }
            assert!(map.len() <= 16, "memory stays the caller's window");
            map.release();
        }
        Ok(())
    });
    assert_eq!(seen, total, "every record, in 16-entry windows");
    assert_eq!(walked as usize, total, "and each one framed exactly once");
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
#[test]
fn the_map_never_allocates_a_buffer_of_its_own() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("buffer.varve");
    build(&path, 100)?;
    let file = MapFormat::open_readonly(&path)?;

    // Pre-sized by the caller, so a map that grew its own storage would have to
    // reallocate this one — and it cannot, it does not own it.
    let mut buffer: Vec<RecordIndexEntry> = Vec::with_capacity(4_096);
    let base = buffer.as_ptr();
    let capacity = buffer.capacity();
    {
        let mut map = file.record_map(&mut buffer)?;
        // Constructing it reads nothing.
        let (_m, walked) = framed(|| map.len());
        assert_eq!(walked, 0);
        map.fill()?;
        assert!(map.len() > 100);
        assert_eq!(
            map.entries().as_ptr(),
            base,
            "still the caller's allocation"
        );
    }
    assert_eq!(buffer.capacity(), capacity, "and never reallocated");
    assert!(!buffer.is_empty(), "the entries are in the caller's buffer");
    buffer.clear();
    Ok(())
}
