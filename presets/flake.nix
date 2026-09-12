{
  description = "Rooms toolchains, independent of the Alpine boot image";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/eaad089433ca2bb662274377d33df3d0e51ef28b";
  outputs = { nixpkgs, ... }:
    let
      systems = [ "aarch64-linux" "x86_64-linux" ];
    in {
      packages = nixpkgs.lib.genAttrs systems (system:
        let
          pkgs = import nixpkgs { inherit system; };
          stacks = {
            rust = [ pkgs.rustc pkgs.cargo pkgs.gcc pkgs.pkg-config ];
            go = [ pkgs.go pkgs.gcc ];
            node = [ pkgs.nodejs pkgs.python3 pkgs.gcc pkgs.gnumake ];
            python = [ pkgs.python3 pkgs.gcc ];
          };
          environment = name: paths: pkgs.buildEnv {
            name = "rooms-${name}";
            inherit paths;
            pathsToLink = [ "/bin" "/lib" "/include" "/share" ];
          };
        in (builtins.mapAttrs environment stacks) // {
          polyglot = environment "polyglot" (nixpkgs.lib.unique
            (builtins.concatLists (builtins.attrValues stacks)));
        });
    };
}
