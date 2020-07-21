use crate::models::unigram::lattice::Lattice;
use crate::models::unigram::trie::{Trie, TrieBuilder};
use crate::tokenizer::{Model, Offsets, Result, Token};
use serde::Deserialize;

use std::collections::HashMap;
use std::convert::TryInto;
use std::error::Error;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

type TokenMap = HashMap<String, u32>;
type Vocab = Vec<String>;

/// A `Unigram` model to encode sentences.
#[derive(Deserialize)]
pub struct Unigram {
    token_to_ids: TokenMap,
    pub(crate) vocab: Vocab,
    pub(super) scores: Vec<f64>,
    #[serde(skip_deserializing, default = "empty_trie")]
    trie: Trie<char>,
    pub min_score: f64,
    bos_id: usize,
    eos_id: usize,
    unk_id: usize,
}
impl std::fmt::Debug for Unigram {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        fmt.debug_struct("BPE")
            .field("vocab", &self.vocab.len())
            .finish()
    }
}

fn empty_trie() -> Trie<char> {
    TrieBuilder::default().build()
}

static K_UNK_PENALTY: f64 = 10.0;

impl Default for Unigram {
    fn default() -> Self {
        let vocab = vec![
            ("<bos>".to_string(), 0.0),
            ("<eos>".to_string(), 0.0),
            ("<unk>".to_string(), 0.0),
        ];
        Self::from(&vocab, 0, 1, 2)
    }
}

/// When loading a `Unigram` model from a file, you might encounter errors.
#[derive(Debug, Clone)]
pub enum LoadError {
    InvalidLine(String),
}
impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Load Error")
    }
}

impl Error for LoadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self)
    }
}

impl Unigram {
    /// Create a `Unigram` model from a given vocabulary.
    /// Vocabulary are the various tokens and their associated score which is a sort of a logprob of
    /// their frequency, which will enable tokenization and sampling.
    /// bos_id, eos_id and unk_id, are the indices of the said tokens within the vocabulary.
    /// For now `Unigram` *requires* at least those 3 tokens because they are used internally.
    /// Further versions might allow that part to be hidden.
    pub fn from(vocabulary: &[(String, f64)], bos_id: usize, eos_id: usize, unk_id: usize) -> Self {
        let n = vocabulary.len();
        let mut vocab: Vec<String> = Vec::with_capacity(n);
        let mut scores: Vec<f64> = Vec::with_capacity(n);
        let mut token_to_ids: TokenMap = HashMap::new();
        let mut builder = TrieBuilder::default();
        assert!(
            n >= 3,
            "We need at least bos, eos, and unk in the vocabulary"
        );
        assert!(bos_id < vocabulary.len(), "Bos id is invalid");
        assert!(eos_id < vocabulary.len(), "Eos id is invalid");
        assert!(unk_id < vocabulary.len(), "Unk id is invalid");
        for (id, (token, score)) in vocabulary.iter().enumerate() {
            vocab.push(token.to_string());
            scores.push(*score);
            token_to_ids.insert(token.to_string(), id as u32);
            let chars: Vec<char> = token.chars().collect();
            builder.push(&chars);
        }
        let min_score = scores.iter().fold(f64::INFINITY, |a, &b| a.min(b));
        if min_score == -f64::INFINITY {
            panic!("Alert min_score !!");
        }
        let trie = builder.build();

        Unigram {
            vocab,
            scores,
            token_to_ids,
            trie,
            min_score,
            bos_id,
            eos_id,
            unk_id,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.vocab.len()
    }

    pub(super) fn populate_nodes(&self, lattice: &mut Lattice) {
        let unk_score = self.min_score - K_UNK_PENALTY;

        let len = lattice.len();

        for begin_pos in 0..len {
            // let now = Instant::now();
            let trie_results: Vec<Vec<char>> =
                self.trie.common_prefix_search(&lattice.chars[begin_pos..]);

            let mut has_single_node = false;

            for result in trie_results {
                // TODO score comes from proto id.
                let n = result.len();
                let tok: String = result.into_iter().collect();
                let id = *self.token_to_ids.get(&tok).unwrap();
                assert_eq!(self.vocab[id as usize], tok);
                let score: f64 = self.scores[id as usize];
                lattice.insert(begin_pos, n, score, id.try_into().unwrap());
                if !has_single_node && n == 1 {
                    has_single_node = true;
                }
            }

            if !has_single_node {
                lattice.insert(begin_pos, 1, unk_score, self.unk_id);
            }
        }
    }

    /// This functions take a String, and will encode it in a Vec of Strings,
    /// of the best tokenization available to the current model. `fuse_unk` is
    /// a flag to decide whether multiple unknown tokens should be fused into a single
    /// unknown model.
    ///
    /// ```
    /// use tokenizers::models::unigram::Unigram;
    ///
    /// let pieces = vec![
    ///     ("<bos>".to_string(), 0.0),
    ///     ("<eos>".to_string(), 0.0),
    ///     ("<unk>".to_string(), 0.0),
    ///     ("a".to_string(), 0.0),
    ///     ("b".to_string(), 0.0),
    ///     ("c".to_string(), 0.0),
    ///     ("d".to_string(), 0.0),
    ///     ("cd".to_string(), 1.0),
    ///     ("ab".to_string(), 2.0),
    ///     ("abc".to_string(), 5.0),
    ///     ("abcd".to_string(), 10.0),
    /// ];
    /// let model = Unigram::from(&pieces, 0, 1, 2);
    /// let result = model.encode("abcdacdxx", false);
    /// assert_eq!(result, vec!["abcd", "a", "cd", "x", "x"]);
    /// let result = model.encode("abcdacdxx", true);
    /// assert_eq!(result, vec!["abcd", "a", "cd", "xx"]);
    /// ```
    pub fn encode(&self, sentence: &str, fuse_unk: bool) -> Vec<String> {
        // TODO optimized version
        // https://github.com/google/sentencepiece/blob/d48247191a6d50e469ed1a4a36e877befffd1851/src/unigram_model.cc#L600
        let mut lattice = Lattice::from(sentence, self.bos_id, self.eos_id, self.unk_id);
        self.populate_nodes(&mut lattice);
        if fuse_unk {
            let mut results = vec![];
            let mut token = String::new();
            for node in lattice.viterbi().iter() {
                let item = lattice.piece(&node.borrow());
                if node.borrow().id == self.unk_id {
                    token.push_str(&item);
                } else {
                    if !token.is_empty() {
                        results.push(token);
                        token = String::new();
                    }
                    results.push(item.to_string());
                }
            }
            if !token.is_empty() {
                results.push(token);
            }
            results
        } else {
            lattice.tokens()
        }
    }

    /// Loads a SentencePiece output model.
    /// In order to get the proper model with spm.
    ///
    /// ```ignore
    /// spm_train --model=unigram --input=.... --model_prefix=myprefix ...
    /// spm_export_vocab --model=myprefix.model --output=myprefix.txt
    /// ```
    ///
    /// After that you can use the model with tokenizers library.
    /// ```no_run
    /// use tokenizers::models::unigram::Unigram;
    /// use std::path::Path;
    ///
    /// let model = Unigram::load_spm(Path::new("myprefix.txt")).unwrap();
    /// ```
    pub fn load_spm(path: &Path) -> Result<Unigram> {
        let file = File::open(path).unwrap();
        let reader = BufReader::new(file);

        // Read the JSON contents of the file as an instance of `User`.
        let mut table: Vec<(String, f64)> = vec![];
        for (i, line) in reader.lines().enumerate() {
            let real_line = line?;
            // ▁ is spm token for space.
            let newline = real_line.replace('▁', " ");
            let tokens: Vec<&str> = newline.split('\t').collect();
            match tokens.as_slice() {
                [token, score] => table.push((token.to_string(), score.parse().unwrap())),
                _ => {
                    return Err(Box::new(LoadError::InvalidLine(format!(
                        "line {} is invalid {:?}",
                        i, real_line
                    ))))
                }
            }
        }
        // XXX: by default in spm unk is 0, bos is 1, eos is 2
        let u = Unigram::from(&table, 1, 2, 0);
        Ok(u)
    }

    /// Iterate of vocabulary of the model as a pair of `(token, score)`.
    pub fn iter(&self) -> UnigramIterator {
        UnigramIterator { model: self, i: 0 }
    }

    /// Loads a SentencePiece output model after being trained by tokenizers.
    /// After that you can use the model with tokenizers library.
    /// ```no_run
    /// use tokenizers::models::unigram::Unigram;
    /// use std::path::Path;
    ///
    /// let model = Unigram::load(Path::new("mymodel-unigram.json")).unwrap();
    /// ```
    pub fn load(path: &Path) -> Result<Unigram> {
        let file = File::open(path).unwrap();
        let reader = BufReader::new(file);

        // Read the JSON contents of the file as an instance of `User`.
        let table: Vec<(String, f64)> = serde_json::from_reader(reader)?;
        let u = Unigram::from(&table, 0, 1, 2);
        Ok(u)
    }
}

/// Iterator to iterate of vocabulary of the model, and their relative score.
pub struct UnigramIterator<'a> {
    model: &'a Unigram,
    i: usize,
}

impl<'a> Iterator for UnigramIterator<'a> {
    type Item = (&'a String, f64);

    fn next(&mut self) -> Option<Self::Item> {
        let i = self.i;
        if i < self.model.len() {
            let r = Some((&self.model.vocab[i], self.model.scores[i]));
            self.i += 1;
            r
        } else {
            None
        }
    }
}

#[typetag::serde]
impl Model for Unigram {
    fn get_vocab(&self) -> &HashMap<String, u32> {
        &self.token_to_ids
    }

    fn get_vocab_size(&self) -> usize {
        self.vocab.len()
    }

    fn tokenize(&self, sentence: Vec<(String, Offsets)>) -> Result<Vec<Token>> {
        // TODO offsets
        let mut results: Vec<Token> = Vec::with_capacity(sentence.len());
        for (element, _) in sentence {
            let tokens = self.encode(&element, false);
            let elts: Vec<Token> = tokens
                .iter()
                .enumerate()
                .map(|(word, string)| {
                    let id = match self.token_to_ids.get(string) {
                        Some(id) => id,
                        None => {
                            // println!("Vocab {:?}", self.vocab);
                            // println!("String {:?} has no id", string);
                            &0
                        }
                    };
                    let offsets = (0, 0);
                    Token::new(*id, string.to_string(), offsets, word as u32)
                })
                .collect();
            results.extend(elts);
        }
        Ok(results)
    }

    fn token_to_id(&self, token: &str) -> Option<u32> {
        self.token_to_ids.get(token).copied()
    }

    fn id_to_token(&self, id: u32) -> Option<&str> {
        match self.vocab.get(id as usize) {
            Some(string) => Some(string),
            None => None,
        }
    }

    fn save(&self, folder: &Path, name: Option<&str>) -> Result<Vec<PathBuf>> {
        let name = match name {
            Some(name) => format!("{}-unigram.json", name),
            None => "unigram.json".to_string(),
        };
        let mut fullpath = PathBuf::new();
        fullpath.push(folder);
        fullpath.push(name);
        let string = serde_json::to_string_pretty(self)?;
        std::fs::write(&fullpath, string)?;
        Ok(vec![fullpath])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_populate_nodes_unk() {
        let pieces = vec![
            ("<s>".to_string(), 0.0),
            ("</s>".to_string(), 0.0),
            ("<unk>".to_string(), 0.0),
        ];
        let model = Unigram::from(&pieces, 0, 1, 2);

        let mut lattice = Lattice::from("abc", 0, 1, 2);
        model.populate_nodes(&mut lattice);

        assert_eq!(lattice.begin_nodes[0].len(), 1);
        assert_eq!(lattice.begin_nodes[1].len(), 1);
        assert_eq!(lattice.begin_nodes[2].len(), 1);
        assert_eq!(lattice.begin_nodes[0][0].borrow().id, 2);
        assert_eq!(lattice.begin_nodes[1][0].borrow().id, 2);
        assert_eq!(lattice.begin_nodes[2][0].borrow().id, 2);
        assert_eq!(lattice.begin_nodes[0][0].borrow().node_id, 2);
        assert_eq!(lattice.begin_nodes[1][0].borrow().node_id, 3);
        assert_eq!(lattice.begin_nodes[2][0].borrow().node_id, 4);
    }

    #[test]
    fn test_populate_nodes() {
        let pieces = vec![
            ("<s>".to_string(), 0.0),
            ("</s>".to_string(), 0.0),
            ("<unk>".to_string(), 0.0),
            ("a".to_string(), 0.1),
            ("b".to_string(), 0.2),
            ("ab".to_string(), 0.3),
            ("bc".to_string(), 0.4),
        ];
        let model = Unigram::from(&pieces, 0, 1, 2);

        let mut lattice = Lattice::from("abc", 0, 1, 2);
        model.populate_nodes(&mut lattice);

        assert_eq!(lattice.begin_nodes[0].len(), 2); // a, ab
        assert_eq!(lattice.begin_nodes[1].len(), 2); // b, bc
        assert_eq!(lattice.begin_nodes[2].len(), 1); // c(unk)

        // Id is the vocabulary id from Unigram model
        // node_id is simply the rank of the given node in the lattice.
        assert_eq!(lattice.begin_nodes[0][0].borrow().id, 3);
        assert_eq!(lattice.begin_nodes[0][1].borrow().id, 5);
        assert_eq!(lattice.begin_nodes[1][0].borrow().id, 4);
        assert_eq!(lattice.begin_nodes[1][1].borrow().id, 6);
        assert_eq!(lattice.begin_nodes[2][0].borrow().id, 2);
        assert_eq!(lattice.begin_nodes[0][0].borrow().node_id, 2);
        assert_eq!(lattice.begin_nodes[0][1].borrow().node_id, 3);
        assert_eq!(lattice.begin_nodes[1][0].borrow().node_id, 4);
        assert_eq!(lattice.begin_nodes[1][1].borrow().node_id, 5);
        assert_eq!(lattice.begin_nodes[2][0].borrow().node_id, 6);
    }

    #[test]
    fn test_encode() {
        let sentencepieces = vec![
            ("<bos>".to_string(), 0.0),
            ("<eos>".to_string(), 0.0),
            ("<unk>".to_string(), 0.0),
            ("a".to_string(), 0.0),
            ("b".to_string(), 0.0),
            ("c".to_string(), 0.0),
            ("d".to_string(), 0.0),
            ("cd".to_string(), 1.0),
            ("ab".to_string(), 2.0),
            ("abc".to_string(), 5.0),
            ("abcd".to_string(), 10.0),
        ];

        let model = Unigram::from(&sentencepieces, 0, 1, 2);
        let result = model.encode("abcd", false);
        assert_eq!(result, vec!["abcd"]);
    }

    #[test]
    fn test_encode2() {
        let sentencepieces = vec![
            ("<bos>".to_string(), 0.0),
            ("<eos>".to_string(), 0.0),
            ("<unk>".to_string(), 0.0),
            ("ab".to_string(), 0.0),
            ("cd".to_string(), -0.1),
            ("abc".to_string(), -0.2),
            ("a".to_string(), -0.3),
            ("b".to_string(), -0.4),
            ("c".to_string(), -0.5),
            ("ABC".to_string(), -0.5),
            ("abcdabcd".to_string(), 20.0), // User defined just max the scores.
            ("q".to_string(), 20.5),
            ("r".to_string(), 20.5),
            ("qr".to_string(), -0.5),
        ];

        let model = Unigram::from(&sentencepieces, 0, 1, 2);
        assert_eq!(model.encode("abc", false), vec!["abc"]);
        assert_eq!(model.encode("AB", false), vec!["A", "B"]);
        assert_eq!(model.encode("AB", true), vec!["AB"]);
        assert_eq!(model.encode("abcd", false), vec!["ab", "cd"]);
        assert_eq!(model.encode("abcc", false), vec!["abc", "c"]);
        assert_eq!(
            model.encode("xabcabaabcdd", false),
            vec!["x", "abc", "ab", "a", "ab", "cd", "d"]
        );
        assert_eq!(
            model.encode("xyz東京", false),
            vec!["x", "y", "z", "東", "京"]
        );

        // User encoded in original version
        assert_eq!(model.encode("ABC", false), vec!["ABC"]);
        assert_eq!(model.encode("abABCcd", false), vec!["ab", "ABC", "cd"]);
        assert_eq!(
            model.encode("ababcdabcdcd", false),
            vec!["ab", "abcdabcd", "cd"]
        );
        assert_eq!(model.encode("abqrcd", false), vec!["ab", "q", "r", "cd"]);
    }
}
