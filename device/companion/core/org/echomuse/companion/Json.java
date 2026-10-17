package org.echomuse.companion;

import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

/**
 * The JSON the local link needs: parse one object, write one flat object.
 * org.json is on the device but not on the JDK the image build runs the
 * codec tests on, so the codec carries its own.
 */
final class Json {
    /** A present JSON null, distinct from an absent key. */
    static final Object NULL = new Object();

    /** The link's objects are flat; this leaves room for future fields. */
    private static final int MAX_DEPTH = 16;

    static final class SyntaxException extends Exception {
        SyntaxException(String message) {
            super(message);
        }
    }

    private final String s;
    private int i;

    private Json(String s) {
        this.s = s;
    }

    /**
     * Parses s, which must hold exactly one JSON object. Values are String,
     * Boolean, Long (integers), Double (other numbers), List, Map or NULL.
     */
    static Map<String, Object> parseObject(String s) throws SyntaxException {
        Json p = new Json(s);
        p.space();
        if (p.peek() != '{') {
            throw p.error("expected an object");
        }
        Map<String, Object> m = p.object(1);
        p.space();
        if (p.i != s.length()) {
            throw p.error("trailing data");
        }
        return m;
    }

    private Object value(int depth) throws SyntaxException {
        if (depth > MAX_DEPTH) {
            throw error("nested too deeply");
        }
        space();
        int c = peek();
        switch (c) {
            case '{':
                return object(depth);
            case '[':
                return array(depth);
            case '"':
                return string();
            case 't':
                literal("true");
                return Boolean.TRUE;
            case 'f':
                literal("false");
                return Boolean.FALSE;
            case 'n':
                literal("null");
                return NULL;
            default:
                if (c == '-' || (c >= '0' && c <= '9')) {
                    return number();
                }
                throw error("unexpected character");
        }
    }

    private Map<String, Object> object(int depth) throws SyntaxException {
        Map<String, Object> m = new HashMap<>();
        i++; // '{'
        space();
        if (peek() == '}') {
            i++;
            return m;
        }
        for (;;) {
            space();
            if (peek() != '"') {
                throw error("expected a key");
            }
            String key = string();
            space();
            expect(':');
            m.put(key, value(depth + 1));
            space();
            if (peek() == ',') {
                i++;
            } else {
                expect('}');
                return m;
            }
        }
    }

    private List<Object> array(int depth) throws SyntaxException {
        List<Object> a = new ArrayList<>();
        i++; // '['
        space();
        if (peek() == ']') {
            i++;
            return a;
        }
        for (;;) {
            a.add(value(depth + 1));
            space();
            if (peek() == ',') {
                i++;
            } else {
                expect(']');
                return a;
            }
        }
    }

    private String string() throws SyntaxException {
        StringBuilder b = new StringBuilder();
        i++; // '"'
        for (;;) {
            if (i >= s.length()) {
                throw error("unterminated string");
            }
            char c = s.charAt(i++);
            if (c == '"') {
                return b.toString();
            }
            if (c < 0x20) {
                throw error("control character in string");
            }
            if (c != '\\') {
                b.append(c);
                continue;
            }
            if (i >= s.length()) {
                throw error("unterminated escape");
            }
            char e = s.charAt(i++);
            switch (e) {
                case '"':
                case '\\':
                case '/':
                    b.append(e);
                    break;
                case 'b':
                    b.append('\b');
                    break;
                case 'f':
                    b.append('\f');
                    break;
                case 'n':
                    b.append('\n');
                    break;
                case 'r':
                    b.append('\r');
                    break;
                case 't':
                    b.append('\t');
                    break;
                case 'u':
                    if (i + 4 > s.length()) {
                        throw error("short \\u escape");
                    }
                    int u = 0;
                    for (int k = 0; k < 4; k++) {
                        int d = Character.digit(s.charAt(i++), 16);
                        if (d < 0) {
                            throw error("bad \\u escape");
                        }
                        u = u << 4 | d;
                    }
                    b.append((char) u);
                    break;
                default:
                    throw error("bad escape");
            }
        }
    }

    private Object number() throws SyntaxException {
        int start = i;
        boolean integral = true;
        if (peek() == '-') {
            i++;
        }
        if (peek() == '0') {
            i++;
        } else if (!digits()) {
            throw error("bad number");
        }
        if (peek() == '.') {
            i++;
            integral = false;
            if (!digits()) {
                throw error("bad fraction");
            }
        }
        if (peek() == 'e' || peek() == 'E') {
            i++;
            integral = false;
            if (peek() == '+' || peek() == '-') {
                i++;
            }
            if (!digits()) {
                throw error("bad exponent");
            }
        }
        String n = s.substring(start, i);
        if (integral) {
            try {
                return Long.parseLong(n);
            } catch (NumberFormatException e) {
                throw error("integer out of range");
            }
        }
        return Double.parseDouble(n);
    }

    private boolean digits() {
        int start = i;
        while (peek() >= '0' && peek() <= '9') {
            i++;
        }
        return i > start;
    }

    private void literal(String word) throws SyntaxException {
        if (!s.startsWith(word, i)) {
            throw error("unexpected character");
        }
        i += word.length();
    }

    private void expect(char c) throws SyntaxException {
        if (peek() != c) {
            throw error("expected '" + c + "'");
        }
        i++;
    }

    private void space() {
        while (peek() == ' ' || peek() == '\t' || peek() == '\n' || peek() == '\r') {
            i++;
        }
    }

    /** The next character, or -1 at the end. */
    private int peek() {
        return i < s.length() ? s.charAt(i) : -1;
    }

    private SyntaxException error(String message) {
        return new SyntaxException(message + " at offset " + i);
    }

    /** Builds one flat object, keys in call order. */
    static final class Writer {
        private final StringBuilder b = new StringBuilder(96).append('{');

        Writer put(String key, String value) {
            key(key);
            if (value == null) {
                b.append("null");
            } else {
                quote(value);
            }
            return this;
        }

        Writer put(String key, long value) {
            key(key);
            b.append(value);
            return this;
        }

        Writer put(String key, boolean value) {
            key(key);
            b.append(value);
            return this;
        }

        String end() {
            return b.append('}').toString();
        }

        private void key(String key) {
            if (b.length() > 1) {
                b.append(',');
            }
            quote(key);
            b.append(':');
        }

        private void quote(String v) {
            b.append('"');
            for (int k = 0; k < v.length(); k++) {
                char c = v.charAt(k);
                switch (c) {
                    case '"':
                        b.append("\\\"");
                        break;
                    case '\\':
                        b.append("\\\\");
                        break;
                    case '\n':
                        b.append("\\n");
                        break;
                    case '\r':
                        b.append("\\r");
                        break;
                    case '\t':
                        b.append("\\t");
                        break;
                    default:
                        if (c < 0x20) {
                            b.append(String.format("\\u%04x", (int) c));
                        } else {
                            b.append(c);
                        }
                }
            }
            b.append('"');
        }
    }
}
