#![allow(unused)]

use super::pfc::*;
use super::wavelettree::*;
use std::io;
use futures::prelude::*;
use futures::future;
use crate::storage::*;
use super::util::*;
use super::bitindex::*;

pub struct MappedPfcDict<M:AsRef<[u8]>+Clone> {
    inner: PfcDict<M>,
    id_wtree: WaveletTree<M>
}

impl<M:AsRef<[u8]>+Clone> MappedPfcDict<M> {
    pub fn from_parts(dict: PfcDict<M>, wtree: WaveletTree<M>) -> MappedPfcDict<M> {
        MappedPfcDict {
            inner: dict,
            id_wtree: wtree
        }
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn get(&self, ix: usize) -> String {
        if ix >= self.len() {
            panic!("index too large for mapped pfc dict");
        }

        let mapped_id = self.id_wtree.lookup_one(ix as u64).unwrap();
        self.inner.get(mapped_id as usize)
    }

    pub fn id(&self, s: &str) -> Option<u64> {
        self.inner.id(s)
            .map(|mapped_id| self.id_wtree.decode_one(mapped_id as usize))
    }
}

pub fn merge_dictionary_stack<F:'static+FileLoad+FileStore>(stack: Vec<(F,Option<BitIndexFiles<F>>)>, dict_files: DictionaryFiles<F>, wavelet_files: BitIndexFiles<F>) -> impl Future<Item=(), Error=io::Error>+Send {
    let dict_builder = PfcDictFileBuilder::new(dict_files.blocks_file.open_write(), dict_files.offsets_file.open_write());

    future::join_all(stack.clone().into_iter().map(|(f, _remap)|dict_file_get_count(f)))
        .and_then(|counts: Vec<u64>| {
            futures::stream::iter_ok(counts.into_iter().scan(0, |mut tally, c| {
                let prev = *tally;
                *tally += c;

                Some(prev)
            })
                                     .zip(stack.into_iter()))
                .and_then(|(offset, (file, remap))| {
                    // TODO this is where we should possibly apply a remapping
                    match remap {
                        None => {
                            let dict_stream = dict_reader_to_stream(file.open_read());
                            let count_stream = futures::stream::unfold(offset, |c| Some(Ok((c, c+1))));
                            future::Either::A(future::ok(Box::new(count_stream.zip(dict_stream)) as Box<dyn Stream<Item=(u64, String),Error=_>+Send>))
                        },
                        Some(remap) => {
                            future::Either::B(dict_file_get_count(file.clone())
                                              .map(|count| (count as f32).log2().ceil() as u8)
                                              .and_then(move |width| future::join_all(vec![remap.bits_file.map(), remap.blocks_file.map(), remap.sblocks_file.map()])
                                                        .map(|maps| BitIndex::from_maps(maps[0].clone(), maps[1].clone(), maps[2].clone()))
                                                        .map(move |bi| WaveletTree::from_parts(bi, width)))
                                              .map(move |wtree| {
                                                  let dict_stream = dict_reader_to_stream(file.open_read());
                                                  //let count_stream = futures::stream::unfold(offset, |c| Some(Ok((c, c+1))));
                                                  let count_stream = futures::stream::iter_ok(wtree.decode());
                                                  Box::new(count_stream.zip(dict_stream)) as Box<dyn Stream<Item=(u64, String),Error=_>+Send>
                                              }))
                        }
                    }

                    //dict_reader_to_indexed_stream(file.open_read(), offset)
                })
                .collect()
        })
        .map(|streams: Vec<_>| sorted_stream(streams, |results| results.iter()
                                     .enumerate()
                                     .filter(|&(_, item)| item.is_some())
                                     .min_by_key(|&(a, item)| &item.unwrap().1)
                                     .map(|x| x.0)))
        .and_then(|stream| stream.fold((dict_builder, Vec::new()), |(builder, mut indexes), (ix, s)| {
            indexes.push(ix);
            builder.add(&s)
                .map(|(_, b)| (b, indexes))
        }))
        .and_then(|(builder, indexes)| {
            let f1 = builder.finalize();
            let max = indexes.iter().max().map(|x|*x).unwrap_or(0) + 1;
            let width = (max as f32).log2().ceil() as u8;
            let stream_constructor = move || futures::stream::iter_ok(indexes.clone());
            let f2 = build_wavelet_tree_from_stream(width, stream_constructor, wavelet_files.bits_file, wavelet_files.blocks_file, wavelet_files.sblocks_file);
            f1.join(f2).map(|_|())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::memory::*;
    use crate::structure::bitindex::*;

    #[test]
    fn create_and_query_mapped_dict() {
        let contents1 = vec![
            "aaaaa",
            "abcdefghijk",
            "arf",
            "bapofsi",
            "berf",
            "bzwas baraf",
            "eadfpoicvu",
            "faadsafdfaf sdfasdf",
            "gahh",
            ];

        let contents2 = vec![
            "aaaaaaaaaa",
            "aaaabbbbbb",
            "addeeerafa",
            "barf",
            "boo boo boo boo",
            "dradsfadfvbbb",
            "eeeee ee e eee",
            "frumps framps fremps",
            "hai hai hai"
            ];

        let blocks1 = MemoryBackedStore::new();
        let offsets1 = MemoryBackedStore::new();
        let builder1 = PfcDictFileBuilder::new(blocks1.open_write(), offsets1.open_write());

        builder1.add_all(contents1.clone().into_iter().map(|s|s.to_string()))
            .and_then(|(_,b)|b.finalize())
            .wait().unwrap();

        let blocks2 = MemoryBackedStore::new();
        let offsets2 = MemoryBackedStore::new();
        let builder2 = PfcDictFileBuilder::new(blocks2.open_write(), offsets2.open_write());

        builder2.add_all(contents2.clone().into_iter().map(|s|s.to_string()))
            .and_then(|(_,b)|b.finalize())
            .wait().unwrap();

        let dict3_files = DictionaryFiles {
            blocks_file: MemoryBackedStore::new(),
            offsets_file: MemoryBackedStore::new()
        };
        let wavelet_files = BitIndexFiles {
            bits_file: MemoryBackedStore::new(),
            blocks_file: MemoryBackedStore::new(),
            sblocks_file: MemoryBackedStore::new()
        };

        merge_dictionary_stack(vec![(blocks1, None), (blocks2, None)], dict3_files.clone(), wavelet_files.clone()).wait().unwrap();

        let dict = PfcDict::parse(dict3_files.blocks_file.map().wait().unwrap(), dict3_files.offsets_file.map().wait().unwrap()).unwrap();
        let wavelet_bitindex = BitIndex::from_maps(wavelet_files.bits_file.map().wait().unwrap(), wavelet_files.blocks_file.map().wait().unwrap(), wavelet_files.sblocks_file.map().wait().unwrap());
        let wavelet_tree = WaveletTree::from_parts(wavelet_bitindex, 5);

        let mapped_dict = MappedPfcDict::from_parts(dict, wavelet_tree);

        let mut total_contents = Vec::with_capacity(contents1.len()+contents2.len());
        total_contents.extend(contents1);
        total_contents.extend(contents2);

        for i in 0..18 {
            let s = mapped_dict.get(i);
            assert_eq!(total_contents[i], s);
            let id = mapped_dict.id(&s).unwrap();
            assert_eq!(i as u64, id);
        }
    }

    #[test]
    fn create_from_mapped_dict_and_query_mapped_dict() {
        let contents1 = vec![
            "aaaaa",
            "abcdefghijk",
            "arf",
            "bapofsi",
            "berf",
            "bzwas baraf",
            "eadfpoicvu",
            "faadsafdfaf sdfasdf",
            "gahh",
            ];

        let contents2 = vec![
            "aaaaaaaaaa",
            "aaaabbbbbb",
            "addeeerafa",
            "barf",
            "boo boo boo boo",
            "dradsfadfvbbb",
            "eeeee ee e eee",
            "frumps framps fremps",
            "hai hai hai"
            ];

        let contents3 = vec![
            "berlin",
            "dodo",
            "fragile"
            ];

        let blocks1 = MemoryBackedStore::new();
        let offsets1 = MemoryBackedStore::new();
        let builder1 = PfcDictFileBuilder::new(blocks1.open_write(), offsets1.open_write());

        builder1.add_all(contents1.clone().into_iter().map(|s|s.to_string()))
            .and_then(|(_,b)|b.finalize())
            .wait().unwrap();

        let blocks2 = MemoryBackedStore::new();
        let offsets2 = MemoryBackedStore::new();
        let builder2 = PfcDictFileBuilder::new(blocks2.open_write(), offsets2.open_write());

        builder2.add_all(contents2.clone().into_iter().map(|s|s.to_string()))
            .and_then(|(_,b)|b.finalize())
            .wait().unwrap();

        let blocks3 = MemoryBackedStore::new();
        let offsets3 = MemoryBackedStore::new();
        let builder3 = PfcDictFileBuilder::new(blocks3.open_write(), offsets3.open_write());

        builder3.add_all(contents3.clone().into_iter().map(|s|s.to_string()))
            .and_then(|(_,b)|b.finalize())
            .wait().unwrap();


        let dict4_files = DictionaryFiles {
            blocks_file: MemoryBackedStore::new(),
            offsets_file: MemoryBackedStore::new()
        };
        let wavelet4_files = BitIndexFiles {
            bits_file: MemoryBackedStore::new(),
            blocks_file: MemoryBackedStore::new(),
            sblocks_file: MemoryBackedStore::new()
        };

        merge_dictionary_stack(vec![(blocks1, None), (blocks2, None)], dict4_files.clone(), wavelet4_files.clone()).wait().unwrap();

        let dict5_files = DictionaryFiles {
            blocks_file: MemoryBackedStore::new(),
            offsets_file: MemoryBackedStore::new()
        };
        let wavelet5_files = BitIndexFiles {
            bits_file: MemoryBackedStore::new(),
            blocks_file: MemoryBackedStore::new(),
            sblocks_file: MemoryBackedStore::new()
        };

        merge_dictionary_stack(vec![(dict4_files.blocks_file, Some(wavelet4_files)), (blocks3, None)], dict5_files.clone(), wavelet5_files.clone()).wait().unwrap();

        let dict = PfcDict::parse(dict5_files.blocks_file.map().wait().unwrap(), dict5_files.offsets_file.map().wait().unwrap()).unwrap();
        let wavelet_bitindex = BitIndex::from_maps(wavelet5_files.bits_file.map().wait().unwrap(), wavelet5_files.blocks_file.map().wait().unwrap(), wavelet5_files.sblocks_file.map().wait().unwrap());
        let wavelet_tree = WaveletTree::from_parts(wavelet_bitindex, 5);

        let mapped_dict = MappedPfcDict::from_parts(dict, wavelet_tree);

        let mut total_contents = Vec::with_capacity(contents1.len()+contents2.len()+contents3.len());
        total_contents.extend(contents1);
        total_contents.extend(contents2);
        total_contents.extend(contents3);

        for i in 0..total_contents.len() {
            let s = mapped_dict.get(i);
            assert_eq!(total_contents[i], s);
            let id = mapped_dict.id(&s).unwrap();
            assert_eq!(i as u64, id);
        }
    }
}
