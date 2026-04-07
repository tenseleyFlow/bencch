! CHECK: three
program test_select
    implicit none
    integer :: x
    x = 3
    select case (x)
    case (1)
        print *, 'one'
    case (2)
        print *, 'two'
    case (3)
        print *, 'three'
    case default
        print *, 'other'
    end select
end program
