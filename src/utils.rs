// MCRW is a extendable management framework for minecraft
// Copyright (C) 2026  YUHAN LI

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

pub fn print_logo() {
    let logo = r#"
                       _      
     _ __ ___   ___ __| |_ __ 
    | '_ ` _ \ / __/ _` | '__|
    | | | | | | (_| (_| | |   
    |_| |_| |_|\___\__,_|_|  
                                         
      Minecraft Rust Wrapper v0.1.0
    "#;
    println!("{}", logo);
}
