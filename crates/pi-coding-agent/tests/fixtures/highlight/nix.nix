{ pkgs ? import <nixpkgs> {}, lib, ... }:

let
  version = "1.0";
  # Build inputs
  inputs = with pkgs; [ git curl ];
in
pkgs.stdenv.mkDerivation rec {
  pname = "demo";
  inherit version;
  src = ./src;
  buildInputs = inputs;
  doCheck = true;
  meta.description = "Demo ${version}";
}
