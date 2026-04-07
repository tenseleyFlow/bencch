! CHECK: positive
program test_if
    implicit none
    integer :: x
    x = 42
    if (x > 0) then
        print *, 'positive'
    else
        print *, 'non-positive'
    end if
end program
