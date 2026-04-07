submodule (parent_mod) child_impl
contains
    module procedure say_hi
        print *, "hi"
    end procedure say_hi
end submodule child_impl
