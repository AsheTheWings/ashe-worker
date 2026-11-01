//! Print the generated `.env`, which lists every input with its development default.

fn main() {
    print!("{}", ashe_worker::inputs::render_env());
}
