#!/bin/sh

mode="bin"
out=""

while [ $# -gt 0 ]; do
  case "$1" in
    -S)
      mode="asm"
      shift
      ;;
    -c)
      mode="obj"
      shift
      ;;
    -o)
      out="$2"
      shift 2
      ;;
    *)
      shift
      ;;
  esac
done

if [ -z "$out" ]; then
  echo "missing output path" >&2
  exit 2
fi

if [ "$mode" = "asm" ]; then
  cat > "$out" <<'EOF'
.arch armv8.5-a
.globl _main
_main:
  ret
EOF
elif [ "$mode" = "obj" ]; then
  printf 'fake object 41\n' > "$out"
else
  cat > "$out" <<'EOF'
#!/bin/sh
printf '41\n'
EOF
  chmod +x "$out"
fi
