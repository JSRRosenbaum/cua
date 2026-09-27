#include "seat_selection_payload.hpp"

#include <cassert>
#include <string>

using cua::hyprland::valid_selection_text;

int main() {
    assert(valid_selection_text("ASCII"));
    assert(valid_selection_text("\xE6\xBC\xA2\xF0\x9F\x99\x82")); // 漢🙂
    assert(valid_selection_text(std::string(900, 'a')));
    assert(!valid_selection_text(std::string(901, 'a')));
    assert(!valid_selection_text(""));
    assert(!valid_selection_text(std::string("a\0b", 3)));
    assert(!valid_selection_text("\xC0\xAF")); // overlong
    assert(!valid_selection_text("\xED\xA0\x80")); // surrogate
    assert(!valid_selection_text("\xF4\x90\x80\x80")); // above U+10FFFF
    assert(!valid_selection_text("\xE6\xBC")); // truncated multibyte
}
