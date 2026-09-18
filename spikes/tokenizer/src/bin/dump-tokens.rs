use rsou_tokenizer_spike::token_spans;

fn main() {
    let text = std::env::args()
        .nth(1)
        .expect("usage: dump_tokens <UTF-8 text>");
    for token in token_spans(&text) {
        println!("{}\t{}\t{}", token.text, token.range.start, token.range.end);
    }
}
