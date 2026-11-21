//! Builds the fuzzy pass's English words: sorted words on stdin, one per
//! line, become an fst set at the path given. `scripts/english-words.sh`
//! runs it.

use std::fs::File;
use std::io::{self, BufRead, BufWriter};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .ok_or("usage: english_fst <output>")?;
    let mut set = fst::SetBuilder::new(BufWriter::new(File::create(path)?))?;
    for word in io::stdin().lock().lines() {
        set.insert(word?)?;
    }
    set.finish()?;
    Ok(())
}
