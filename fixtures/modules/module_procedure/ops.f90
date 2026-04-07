module ops
    implicit none
contains
    integer function add_one(x)
        integer, intent(in) :: x

        add_one = x + 1
    end function add_one
end module ops
