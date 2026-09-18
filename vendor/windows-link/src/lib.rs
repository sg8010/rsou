#![doc = include_str!("../readme.md")]
#![no_std]

/// Defines an external function to import.
#[cfg(all(windows, target_arch = "x86"))]
#[macro_export]
macro_rules! link {
    // CoTaskMemFree is exported by ole32.dll on Windows 7. windows-sys 0.61
    // describes it as combase.dll, which is not a Windows 7 system DLL.
    ("combase.dll" $abi:literal fn CoTaskMemFree($($signature:tt)*) ) => (
        #[link(name = "ole32.dll", kind = "raw-dylib", modifiers = "+verbatim", import_name_type = "undecorated")]
        extern $abi {
            pub fn CoTaskMemFree($($signature)*);
        }
    );
    ($library:literal $abi:literal $($link_name:literal)? fn $($function:tt)*) => (
        #[link(name = $library, kind = "raw-dylib", modifiers = "+verbatim", import_name_type = "undecorated")]
        extern $abi {
            $(#[link_name=$link_name])?
            pub fn $($function)*;
        }
    )
}

/// Defines an external function to import.
#[cfg(all(windows, not(target_arch = "x86")))]
#[macro_export]
macro_rules! link {
    // CoTaskMemFree is exported by ole32.dll on Windows 7. windows-sys 0.61
    // describes it as combase.dll, which is not a Windows 7 system DLL.
    ("combase.dll" $abi:literal fn CoTaskMemFree($($signature:tt)*) ) => (
        #[link(name = "ole32.dll", kind = "raw-dylib", modifiers = "+verbatim")]
        extern $abi {
            pub fn CoTaskMemFree($($signature)*);
        }
    );
    ($library:literal $abi:literal $($link_name:literal)? fn $($function:tt)*) => (
        #[link(name = $library, kind = "raw-dylib", modifiers = "+verbatim")]
        extern $abi {
            $(#[link_name=$link_name])?
            pub fn $($function)*;
        }
    )
}

/// Defines an external function to import.
#[cfg(not(windows))]
#[macro_export]
macro_rules! link {
    ($library:literal $abi:literal $($link_name:literal)? fn $($function:tt)*) => (
        extern $abi {
            pub fn $($function)*;
        }
    )
}
