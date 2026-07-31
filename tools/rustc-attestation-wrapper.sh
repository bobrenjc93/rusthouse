#!/bin/sh
set -eu

configuration=
pending=
first=true
source_path=

append_configuration() {
    if [ -z "$configuration" ]; then
        configuration=$1
    else
        configuration="$configuration
$1"
    fi
}

for argument in "$@"; do
    if [ "$first" = true ]; then
        first=false
        continue
    fi

    if [ -n "$pending" ]; then
        case "$pending" in
            -C)
                case "$argument" in
                    metadata=*|extra-filename=*|incremental=*) ;;
                    *) append_configuration "-C$argument" ;;
                esac
                ;;
            -Z|--cfg|--target|--crate-type|--edition|--check-cfg)
                append_configuration "$pending=$argument"
                ;;
        esac
        pending=
        continue
    fi

    case "$argument" in
        @*)
            echo "rustc response files are unsupported by the RustHouse attestation wrapper" >&2
            exit 1
            ;;
        -C|-Z|--cfg|--target|--crate-type|--edition|--check-cfg)
            pending=$argument
            ;;
        -Cmetadata=*|-Cextra-filename=*|-Cincremental=*) ;;
        -C*|-Z*|-O|-g|--test|--cfg=*|--target=*|--crate-type=*|--edition=*|--check-cfg=*)
            append_configuration "$argument"
            ;;
        *.rs)
            source_path=$argument
            ;;
    esac
done

if [ -n "$pending" ]; then
    echo "rustc argument $pending is missing its value" >&2
    exit 1
fi
if [ -z "$source_path" ]; then
    exec "$@"
fi

encoded_configuration=$(printf '%s' "$configuration" | od -An -tx1 | tr -d ' \n')
exec "$@" \
    --cfg rusthouse_final_rustc_attested \
    "--remap-path-prefix=$source_path=rusthouse-final-rustc-$encoded_configuration"
